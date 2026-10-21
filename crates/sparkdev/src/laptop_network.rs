//! Read-only Windows route and interface observations. Link rate is not
//! measured throughput, and an adapter name cannot prove its USB bus speed.
use serde_json::{Value, json};

#[derive(Clone, Debug)]
struct Interface {
    index: u32,
    name: String,
    description: String,
    kind: &'static str,
    transmit_link_speed_mbps: Option<u64>,
}
fn report(interfaces: Option<Vec<Interface>>, selected: Option<u32>) -> Value {
    let available = interfaces.is_some();
    let interface = interfaces
        .as_ref()
        .and_then(|list| list.iter().find(|i| Some(i.index) == selected));
    let usb = interface.is_some_and(|i| i.description.to_ascii_lowercase().contains("usb"));
    let usb_advice = if usb {
        Some(
            "This adapter's description mentions USB. If it is connected through USB 2.0, the USB bus can limit transfers even when Ethernet reports a faster link rate. Try a USB 3.x port and compatible adapter. Windows' network interface report does not tell DGXBridge the negotiated USB generation.",
        )
    } else {
        None
    };
    json!({"available":available,"route_known":interface.is_some(),
        "interface":interface.map(|i|json!({"name":i.name,"description":i.description,"kind":i.kind,
            "transmit_link_speed_mbps":i.transmit_link_speed_mbps,"usb_adapter_hint":usb})),
        "usb_advice":usb_advice,"speed_is_link_rate":true})
}
/// No packets or probes: query Windows' route table and adapter APIs. An
/// unresolved hostname, a paired route or unsupported OS reports unknown.
#[cfg(windows)]
pub fn info(host: Option<&str>) -> Value {
    let selected = host
        .and_then(|h| h.parse::<std::net::Ipv4Addr>().ok())
        .and_then(crate::discover::routed_via);
    report(windows::interfaces(), selected)
}
#[cfg(not(windows))]
pub fn info(_: Option<&str>) -> Value {
    report(None, None)
}

#[cfg(windows)]
mod windows {
    use super::Interface;
    use windows_sys::Win32::{
        Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_NO_DATA, NO_ERROR},
        NetworkManagement::IpHelper::{
            GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
            GAA_FLAG_SKIP_UNICAST, GetAdaptersAddresses, GetIfEntry2, IP_ADAPTER_ADDRESSES_LH,
            MIB_IF_ROW2,
        },
        NetworkManagement::Ndis::IfOperStatusUp,
        Networking::WinSock::AF_INET,
    };
    unsafe fn wide(text: *const u16) -> String {
        if text.is_null() {
            return String::new();
        }
        let mut len = 0;
        while len < 512 && unsafe { *text.add(len) } != 0 {
            len += 1;
        }
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) })
    }
    pub(super) fn interfaces() -> Option<Vec<Interface>> {
        let mut size = 15 * 1024u32;
        for _ in 0..4 {
            let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
            let mut length = u32::try_from(buffer.len() * 8).ok()?;
            let result = unsafe {
                GetAdaptersAddresses(
                    u32::from(AF_INET),
                    GAA_FLAG_SKIP_UNICAST
                        | GAA_FLAG_SKIP_ANYCAST
                        | GAA_FLAG_SKIP_MULTICAST
                        | GAA_FLAG_SKIP_DNS_SERVER,
                    std::ptr::null(),
                    buffer.as_mut_ptr().cast(),
                    &mut length,
                )
            };
            if result == ERROR_NO_DATA {
                return Some(Vec::new());
            }
            if result == ERROR_BUFFER_OVERFLOW && length <= 1024 * 1024 {
                size = length.max(size);
                continue;
            }
            if result != NO_ERROR {
                return None;
            }
            let mut list = Vec::new();
            let mut node = buffer.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
            for _ in 0..256 {
                let Some(adapter) = (unsafe { node.as_ref() }) else {
                    break;
                };
                let index = unsafe { adapter.Anonymous1.Anonymous.IfIndex };
                if adapter.OperStatus == IfOperStatusUp {
                    let mut row = MIB_IF_ROW2 {
                        InterfaceIndex: index,
                        ..Default::default()
                    };
                    let speed = if unsafe { GetIfEntry2(&mut row) } == NO_ERROR {
                        row.TransmitLinkSpeed
                    } else {
                        adapter.TransmitLinkSpeed
                    };
                    list.push(Interface {
                        index,
                        name: unsafe { wide(adapter.FriendlyName) },
                        description: unsafe { wide(adapter.Description) },
                        kind: match adapter.IfType {
                            6 => "wired",
                            71 => "wifi",
                            _ => "other",
                        },
                        transmit_link_speed_mbps: (speed > 0).then_some(speed / 1_000_000),
                    });
                }
                node = adapter.Next.cast_const();
            }
            return Some(list);
        }
        None
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn route_is_selected_by_interface_index_and_usb_generation_is_not_guessed() {
        let adapters = vec![
            Interface {
                index: 7,
                name: "Ethernet".into(),
                description: "USB 2.5GbE adapter".into(),
                kind: "wired",
                transmit_link_speed_mbps: Some(2500),
            },
            Interface {
                index: 8,
                name: "Wi-Fi".into(),
                description: "Wireless adapter".into(),
                kind: "wifi",
                transmit_link_speed_mbps: Some(866),
            },
        ];
        let cable = report(Some(adapters.clone()), Some(7));
        assert_eq!(cable["interface"]["transmit_link_speed_mbps"], 2500);
        assert!(
            cable["usb_advice"]
                .as_str()
                .unwrap()
                .contains("does not tell")
        );
        let wifi = report(Some(adapters), Some(8));
        assert_eq!(wifi["interface"]["kind"], "wifi");
        assert!(wifi["usb_advice"].is_null());
        let unknown = report(None, Some(7));
        assert_eq!(unknown["available"], false);
        assert!(unknown["interface"].is_null());
    }
}
