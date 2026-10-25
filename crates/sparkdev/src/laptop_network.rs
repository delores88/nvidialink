//! Read-only Windows route and interface observations. Link rate is not
//! measured throughput. A USB adapter's bus speed is read from its hub port,
//! never guessed from its name.
use serde_json::{Value, json};

#[derive(Clone, Debug)]
struct Interface {
    index: u32,
    name: String,
    description: String,
    kind: &'static str,
    transmit_link_speed_mbps: Option<u64>,
    /// What the adapter's USB hub port reports: `usb3`, `usb2_port` (a USB 3
    /// adapter running at USB 2.0 speed) or `usb2_device` (a USB 2.0-only
    /// adapter). None when it is not on USB or the hub could not be asked.
    usb_bus: Option<&'static str>,
}
const USB2_PORT_ADVICE: &str = "This USB network adapter supports USB 3 but is running at USB 2.0 speed, which caps transfers at about 300 Mbit/s. Plug it straight into a USB 3 or USB-C port on the laptop, not through a USB 2.0 hub or cable.";
const USB2_DEVICE_ADVICE: &str = "This network adapter is USB 2.0 only, which caps transfers at about 300 Mbit/s. A USB 3 adapter in a USB 3 or USB-C port is much faster.";
fn report(interfaces: Option<Vec<Interface>>, selected: Option<u32>) -> Value {
    let available = interfaces.is_some();
    let interface = interfaces
        .as_ref()
        .and_then(|list| list.iter().find(|i| Some(i.index) == selected));
    let description = interface.map(|i| i.description.to_ascii_lowercase());
    let usb = description.as_ref().is_some_and(|d| d.contains("usb"))
        || interface.is_some_and(|i| i.usb_bus.is_some());
    // Only the hub's answer: an unobserved bus stays unknown, whatever the name says.
    let usb_bus = interface.and_then(|i| i.usb_bus);
    let usb_advice = match usb_bus {
        Some("usb2_port") => Some(USB2_PORT_ADVICE),
        Some("usb2_device") => Some(USB2_DEVICE_ADVICE),
        _ => None,
    };
    json!({"available":available,"route_known":interface.is_some(),
        "interface":interface.map(|i|json!({"name":i.name,"description":i.description,"kind":i.kind,
            "transmit_link_speed_mbps":i.transmit_link_speed_mbps,"usb_adapter_hint":usb,
            "usb_bus":usb_bus.or(usb.then_some("unknown"))})),
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
        Devices::{
            DeviceAndDriverInstallation::{
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_DevNode_PropertyW, CM_Get_Device_IDW,
                CM_Get_Device_Interface_List_SizeW, CM_Get_Device_Interface_ListW, CM_Get_Parent,
                CM_LOCATE_DEVNODE_NORMAL, CM_Locate_DevNodeW, CR_SUCCESS,
            },
            Properties::{DEVPKEY_Device_Address, DEVPROP_TYPE_UINT32},
            Usb::{
                GUID_DEVINTERFACE_USB_HUB, IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX_V2,
                USB_NODE_CONNECTION_INFORMATION_EX_V2,
            },
        },
        Foundation::{
            CloseHandle, ERROR_BUFFER_OVERFLOW, ERROR_NO_DATA, ERROR_SUCCESS, GENERIC_WRITE,
            INVALID_HANDLE_VALUE, NO_ERROR,
        },
        NetworkManagement::IpHelper::{
            GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
            GAA_FLAG_SKIP_UNICAST, GetAdaptersAddresses, GetIfEntry2, IP_ADAPTER_ADDRESSES_LH,
            MIB_IF_ROW2,
        },
        NetworkManagement::Ndis::IfOperStatusUp,
        Networking::WinSock::AF_INET,
        Storage::FileSystem::{CreateFileW, FILE_SHARE_WRITE, OPEN_EXISTING},
        System::{
            IO::DeviceIoControl,
            Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW},
        },
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
                    let kind = match adapter.IfType {
                        6 => "wired",
                        71 => "wifi",
                        _ => "other",
                    };
                    list.push(Interface {
                        index,
                        name: unsafe { wide(adapter.FriendlyName) },
                        description: unsafe { wide(adapter.Description) },
                        kind,
                        transmit_link_speed_mbps: (speed > 0).then_some(speed / 1_000_000),
                        usb_bus: if kind == "wired" {
                            unsafe { narrow(adapter.AdapterName) }.and_then(|guid| usb_bus(&guid))
                        } else {
                            None
                        },
                    });
                }
                node = adapter.Next.cast_const();
            }
            return Some(list);
        }
        None
    }
    unsafe fn narrow(text: *const u8) -> Option<String> {
        if text.is_null() {
            return None;
        }
        let mut len = 0;
        while len < 64 && unsafe { *text.add(len) } != 0 {
            len += 1;
        }
        String::from_utf8(unsafe { std::slice::from_raw_parts(text, len) }.to_vec()).ok()
    }
    fn utf16(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }
    /// The adapter's USB generation from its parent hub port, or None when it is
    /// not on USB or any step is refused. Read-only: registry, device tree, one IOCTL.
    fn usb_bus(adapter_guid: &str) -> Option<&'static str> {
        let guid = adapter_guid.as_bytes();
        if guid.len() != 38
            || guid[0] != b'{'
            || guid[37] != b'}'
            || !guid[1..37]
                .iter()
                .all(|c| c.is_ascii_hexdigit() || *c == b'-')
        {
            return None;
        }
        let key = utf16(&format!(
            r"SYSTEM\CurrentControlSet\Control\Network\{{4D36E972-E325-11CE-BFC1-08002BE10318}}\{adapter_guid}\Connection"
        ));
        let value = utf16("PnPInstanceId");
        let mut instance = vec![0u16; 512];
        let mut size = u32::try_from(instance.len() * 2).ok()?;
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                instance.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if status != ERROR_SUCCESS
            || !String::from_utf16_lossy(&instance[..4]).eq_ignore_ascii_case("USB\\")
        {
            return None;
        }
        let mut node = 0u32;
        if unsafe { CM_Locate_DevNodeW(&mut node, instance.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) }
            != CR_SUCCESS
        {
            return None;
        }
        // A composite adapter's network function sits one level below the device the hub sees.
        for _ in 0..4 {
            let mut parent = 0u32;
            if unsafe { CM_Get_Parent(&mut parent, node, 0) } != CR_SUCCESS {
                return None;
            }
            if let Some(hub) = hub_path(parent) {
                return port_speed(&hub, port(node)?);
            }
            node = parent;
        }
        None
    }
    /// The USB hub device interface of `node`, if it is a hub.
    fn hub_path(node: u32) -> Option<Vec<u16>> {
        let mut id = vec![0u16; 512];
        let capacity = u32::try_from(id.len() - 1).ok()?;
        if unsafe { CM_Get_Device_IDW(node, id.as_mut_ptr(), capacity, 0) } != CR_SUCCESS {
            return None;
        }
        let mut len = 0u32;
        let found = unsafe {
            CM_Get_Device_Interface_List_SizeW(
                &mut len,
                &GUID_DEVINTERFACE_USB_HUB,
                id.as_ptr(),
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        if found != CR_SUCCESS || !(2..=4096).contains(&len) {
            return None;
        }
        let mut list = vec![0u16; len as usize];
        let listed = unsafe {
            CM_Get_Device_Interface_ListW(
                &GUID_DEVINTERFACE_USB_HUB,
                id.as_ptr(),
                list.as_mut_ptr(),
                len,
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        let end = list.iter().position(|&c| c == 0)?;
        if listed != CR_SUCCESS || end == 0 {
            return None;
        }
        list.truncate(end + 1);
        Some(list)
    }
    /// A USB device's port number on its hub.
    fn port(node: u32) -> Option<u32> {
        let (mut kind, mut value, mut size) = (0u32, 0u32, 4u32);
        let read = unsafe {
            CM_Get_DevNode_PropertyW(
                node,
                &DEVPKEY_Device_Address,
                &mut kind,
                (&raw mut value).cast(),
                &mut size,
                0,
            )
        };
        (read == CR_SUCCESS && kind == DEVPROP_TYPE_UINT32 && size == 4 && value > 0)
            .then_some(value)
    }
    fn port_speed(hub: &[u16], port: u32) -> Option<&'static str> {
        let handle = unsafe {
            CreateFileW(
                hub.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let size =
            u32::try_from(std::mem::size_of::<USB_NODE_CONNECTION_INFORMATION_EX_V2>()).ok()?;
        let mut info = USB_NODE_CONNECTION_INFORMATION_EX_V2 {
            ConnectionIndex: port,
            Length: size,
            ..Default::default()
        };
        info.SupportedUsbProtocols.ul = 0b111; // USB 1.1, 2.0 and 3.x.
        let mut returned = 0u32;
        let pointer = &raw mut info;
        let asked = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX_V2,
                pointer.cast_const().cast(),
                size,
                pointer.cast(),
                size,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        unsafe { CloseHandle(handle) };
        if asked == 0 || returned < size {
            return None;
        }
        // Bit 0: operating at SuperSpeed or faster. Bit 1: capable of it.
        let flags = unsafe { info.Flags.ul };
        Some(if flags & 1 != 0 {
            "usb3"
        } else if flags & 2 != 0 {
            "usb2_port"
        } else {
            "usb2_device"
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn adapter(
        index: u32,
        description: &str,
        kind: &'static str,
        usb_bus: Option<&'static str>,
    ) -> Interface {
        Interface {
            index,
            name: format!("Adapter {index}"),
            description: description.into(),
            kind,
            transmit_link_speed_mbps: Some(1000),
            usb_bus,
        }
    }
    #[test]
    fn route_is_selected_by_interface_index() {
        let adapters = vec![
            adapter(7, "USB 2.5GbE adapter", "wired", None),
            adapter(8, "Wireless adapter", "wifi", None),
        ];
        let cable = report(Some(adapters.clone()), Some(7));
        assert_eq!(cable["interface"]["transmit_link_speed_mbps"], 1000);
        let wifi = report(Some(adapters), Some(8));
        assert_eq!(wifi["interface"]["kind"], "wifi");
        assert!(wifi["interface"]["usb_bus"].is_null());
        assert!(wifi["usb_advice"].is_null());
        let unknown = report(None, Some(7));
        assert_eq!(unknown["available"], false);
        assert!(unknown["interface"].is_null());
    }
    #[test]
    fn usb_advice_follows_the_hub_port_never_the_adapter_name() {
        let pick = |a: Interface| report(Some(vec![a]), Some(1));
        // A USB 3 adapter on a USB 3 port: no warning, whatever its name says.
        let fast = pick(adapter(
            1,
            "Realtek Gaming USB 2.5GbE Family Controller",
            "wired",
            Some("usb3"),
        ));
        assert_eq!(fast["interface"]["usb_bus"], "usb3");
        assert!(fast["usb_advice"].is_null());
        // The hub could not be asked: "USB" in the name alone is no longer a warning.
        let unasked = pick(adapter(
            1,
            "Realtek Gaming USB 2.5GbE Family Controller",
            "wired",
            None,
        ));
        assert_eq!(unasked["interface"]["usb_bus"], "unknown");
        assert!(unasked["usb_advice"].is_null());
        // A USB 3 adapter held back by a USB 2.0 port or hub.
        let held = pick(adapter(1, "USB 2.5GbE adapter", "wired", Some("usb2_port")));
        assert!(
            held["usb_advice"]
                .as_str()
                .unwrap()
                .contains("USB 3 or USB-C port")
        );
        // A USB 2.0-only adapter its hub reports. Unasked, even a name saying
        // USB 2.0 asserts nothing about the hardware.
        let old = pick(adapter(1, "USB adapter", "wired", Some("usb2_device")));
        assert!(old["usb_advice"].as_str().unwrap().contains("USB 2.0 only"));
        let named = pick(adapter(1, "USB2.0 Ethernet Adapter", "wired", None));
        assert_eq!(named["interface"]["usb_bus"], "unknown");
        assert!(named["usb_advice"].is_null());
        // Not USB at all.
        let pci = pick(adapter(
            1,
            "Intel Ethernet Controller I226-V",
            "wired",
            None,
        ));
        assert!(pci["interface"]["usb_bus"].is_null());
        assert_eq!(pci["interface"]["usb_adapter_hint"], false);
    }
}
