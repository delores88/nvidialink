//! Conservative, read-only Windows Firewall evidence for the current link binary.
//! A failed connection or default inbound policy is never treated as proof.
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub observed: bool,
    pub enabled: bool,
    pub profile: Profile,
    pub inbound_blocks: u16,
    pub outbound_blocks: u16,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    Private,
    Public,
    Domain,
    #[default]
    Unknown,
}

impl Observation {
    pub fn known_block(&self) -> bool {
        self.observed && self.enabled && self.profile != Profile::Unknown
            && self.inbound_blocks <= 256 && self.outbound_blocks <= 256
            && (self.inbound_blocks > 0 || self.outbound_blocks > 0)
    }
}

#[cfg(any(windows, test))]
const QUERY: &str = r#"
$ErrorActionPreference='Stop'
[Console]::OutputEncoding=New-Object Text.UTF8Encoding $false
$unknown=@{observed=$false;enabled=$false;profile='unknown';inbound_blocks=0;outbound_blocks=0}
function Any-Scope($value) { $v=@($value); return $v.Count -eq 1 -and [string]$v[0] -eq 'Any' }
function Applies($rule,$bit) { return [int]$rule.Profile -eq 0 -or ([int]$rule.Profile -band $bit) -ne 0 }
try {
  $program=[IO.Path]::GetFullPath($env:RTXBRIDGE_LINK_BINARY)
  $route=@(Find-NetRoute -RemoteIPAddress $env:RTXBRIDGE_LINK_TARGET)
  $index=@($route | Select-Object -ExpandProperty InterfaceIndex -Unique)
  if ($index.Count -ne 1) { throw 'route unavailable' }
  $network=@(Get-NetConnectionProfile -InterfaceIndex $index[0])
  if ($network.Count -ne 1) { throw 'profile unavailable' }
  $name=switch ([string]$network[0].NetworkCategory) { 'Private' {'Private'} 'Public' {'Public'} 'DomainAuthenticated' {'Domain'} default {throw 'profile unavailable'} }
  $bit=switch ($name) { 'Domain' {1} 'Private' {2} 'Public' {4} }
  $policy=@(Get-NetFirewallProfile -PolicyStore ActiveStore -Name $name)
  if ($policy.Count -ne 1) { throw 'policy unavailable' }
  $enabled=[string]$policy[0].Enabled -eq 'True'
  $inbound=0; $outbound=0
  if ($enabled) {
    # Authenticated bypass policy can override block rules. Ambiguous policy is unknown.
    $bypass=@(Get-NetFirewallSecurityFilter -PolicyStore ActiveStore -OverrideBlockRules $true | Get-NetFirewallRule | Where-Object { [string]$_.Enabled -eq 'True' -and (Applies $_ $bit) })
    if ($bypass.Count) { throw 'authenticated bypass policy requires review' }
    $filters=@(Get-NetFirewallApplicationFilter -PolicyStore ActiveStore -Program $program)
    foreach ($app in $filters) {
      $actual=[IO.Path]::GetFullPath([Environment]::ExpandEnvironmentVariables([string]$app.Program))
      if (-not [StringComparer]::OrdinalIgnoreCase.Equals($actual,$program)) { continue }
      foreach ($rule in @($app | Get-NetFirewallRule)) {
        if ([string]$rule.Enabled -ne 'True' -or [string]$rule.Action -ne 'Block' -or -not (Applies $rule $bit)) { continue }
        $port=@($rule | Get-NetFirewallPortFilter)
        $address=@($rule | Get-NetFirewallAddressFilter)
        $iface=@($rule | Get-NetFirewallInterfaceFilter)
        $type=@($rule | Get-NetFirewallInterfaceTypeFilter)
        $service=@($rule | Get-NetFirewallServiceFilter)
        $security=@($rule | Get-NetFirewallSecurityFilter)
        if ($port.Count -ne 1 -or $address.Count -ne 1 -or $iface.Count -ne 1 -or $type.Count -ne 1 -or $service.Count -ne 1 -or $security.Count -ne 1) { continue }
        if (@('UDP','17','Any','256') -notcontains [string]$port[0].Protocol) { continue }
        if (-not (Any-Scope $port[0].LocalPort) -or -not (Any-Scope $port[0].RemotePort)) { continue }
        if (-not (Any-Scope $address[0].LocalAddress) -or -not (Any-Scope $address[0].RemoteAddress)) { continue }
        if (-not (Any-Scope $iface[0].InterfaceAlias) -or -not (Any-Scope $type[0].InterfaceType) -or -not (Any-Scope $service[0].Service)) { continue }
        if (-not (Any-Scope $security[0].LocalUser) -or -not (Any-Scope $security[0].RemoteUser) -or -not (Any-Scope $security[0].RemoteMachine)) { continue }
        if ([string]$rule.Direction -eq 'Inbound') { $inbound++ }
        elseif ([string]$rule.Direction -eq 'Outbound') { $outbound++ }
        if ($inbound -gt 256 -or $outbound -gt 256) { throw 'too many matching rules' }
      }
    }
  }
  @{observed=$true;enabled=$enabled;profile=$name.ToLowerInvariant();inbound_blocks=$inbound;outbound_blocks=$outbound} | ConvertTo-Json -Compress
} catch { $unknown | ConvertTo-Json -Compress }
"#;

/// This reads local effective rules only. It never changes a profile, rule or policy.
#[cfg(windows)]
pub async fn observe(target: IpAddr) -> Observation {
    use std::{path::Path, process::Stdio, time::Duration};
    let Ok(binary) = std::env::current_exe() else { return Observation::default() };
    if !binary.is_absolute() || !binary.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("sparkdev-link.exe")) {
        return Observation::default();
    }
    let Some(system) = std::env::var_os("SystemRoot") else { return Observation::default() };
    let powershell = Path::new(&system).join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut command = tokio::process::Command::new(powershell);
    command.args(["-NoProfile", "-NonInteractive", "-Command", QUERY])
        .env("RTXBRIDGE_LINK_BINARY", binary).env("RTXBRIDGE_LINK_TARGET", target.to_string())
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null())
        .kill_on_drop(true).creation_flags(0x08000000);
    let Ok(child) = command.spawn() else { return Observation::default() };
    let Ok(Ok(output)) = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output()).await else {
        return Observation::default();
    };
    if !output.status.success() || output.stdout.len() > 16 * 1024 { return Observation::default() }
    serde_json::from_slice(&output.stdout).unwrap_or_default()
}

#[cfg(not(windows))]
pub async fn observe(_: IpAddr) -> Observation { Observation::default() }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_enabled_profile_scoped_explicit_blocks_are_positive_evidence() {
        let known = Observation { observed:true,enabled:true,profile:Profile::Private,inbound_blocks:1,outbound_blocks:0 };
        assert!(known.known_block());
        for observation in [Observation::default(),Observation { observed:false,..known.clone() },Observation { enabled:false,..known.clone() },Observation { profile:Profile::Unknown,..known.clone() },Observation { inbound_blocks:0,..known.clone() },Observation { inbound_blocks:257,..known }] {
            assert!(!observation.known_block());
        }
    }
    #[test]
    fn query_reads_exact_active_policy_without_changing_rules_or_networks() {
        for required in ["PolicyStore ActiveStore", "Get-NetFirewallApplicationFilter", "OrdinalIgnoreCase.Equals", "Find-NetRoute", "Get-NetConnectionProfile", "'Block'", "'UDP'", "OverrideBlockRules", "Any-Scope"] {
            assert!(QUERY.contains(required),"{required}");
        }
        for mutation in ["New-Net", "Set-Net", "Remove-Net", "Enable-Net", "Disable-Net", "Start-Process", "RunAs", "Invoke-Expression"] {
            assert!(!QUERY.contains(mutation),"{mutation}");
        }
        assert!(!QUERY.contains("DefaultInboundAction"),"Default deny is not evidence that a solicited reply was blocked");
    }
    #[test]
    fn unknown_or_malformed_query_output_is_not_a_firewall_diagnosis() {
        for value in [r#"{"observed":true,"enabled":true}"#,r#"{"observed":true,"enabled":true,"profile":"private","inbound_blocks":1,"outbound_blocks":0,"extra":"untrusted"}"#] {
            assert!(serde_json::from_str::<Observation>(value).is_err());
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_query_uses_effective_exact_program_udp_rules_and_current_profile() {
        use serde_json::json;
        use std::{os::windows::process::CommandExt,path::Path,process::{Command,Stdio}};
        // Shadow every network-policy cmdlet with a local fixture. The actual
        // query still runs, but no real adapter or firewall policy is queried.
        const FIXTURE:&str=r#"
$script:f=$env:RTXBRIDGE_FIREWALL_FIXTURE | ConvertFrom-Json
function Find-NetRoute { param($RemoteIPAddress) [pscustomobject]@{InterfaceIndex=7} }
function Get-NetConnectionProfile { param($InterfaceIndex) [pscustomobject]@{NetworkCategory=$script:f.category} }
function Get-NetFirewallProfile { param($PolicyStore,$Name) if ($script:f.queryFails) {throw 'fixture failure'}; [pscustomobject]@{Enabled=$script:f.profileEnabled} }
function Get-NetFirewallApplicationFilter { param($PolicyStore,$Program) [pscustomobject]@{Kind='app';Program=$script:f.program} }
function Get-NetFirewallRule { param([Parameter(ValueFromPipeline)]$InputObject) process {
 if ($InputObject.Kind -eq 'bypass') {[pscustomobject]@{Enabled='True';Profile=0}}
 else {[pscustomobject]@{Enabled=$script:f.enabled;Profile=$script:f.profile;Action=$script:f.action;Direction=$script:f.direction}}
} }
function Get-NetFirewallPortFilter { param([Parameter(ValueFromPipeline)]$InputObject) process {[pscustomobject]@{Protocol=$script:f.protocol;LocalPort=$script:f.localPort;RemotePort='Any'}} }
function Get-NetFirewallAddressFilter { param([Parameter(ValueFromPipeline)]$InputObject) process {[pscustomobject]@{LocalAddress='Any';RemoteAddress='Any'}} }
function Get-NetFirewallInterfaceFilter { param([Parameter(ValueFromPipeline)]$InputObject) process {[pscustomobject]@{InterfaceAlias='Any'}} }
function Get-NetFirewallInterfaceTypeFilter { param([Parameter(ValueFromPipeline)]$InputObject) process {[pscustomobject]@{InterfaceType='Any'}} }
function Get-NetFirewallServiceFilter { param([Parameter(ValueFromPipeline)]$InputObject) process {[pscustomobject]@{Service='Any'}} }
function Get-NetFirewallSecurityFilter { param($PolicyStore,$OverrideBlockRules,[Parameter(ValueFromPipeline)]$InputObject) process {
 if ($PSBoundParameters.ContainsKey('OverrideBlockRules')) {if ($script:f.bypass) {[pscustomobject]@{Kind='bypass'}};return}
 [pscustomobject]@{LocalUser='Any';RemoteUser='Any';RemoteMachine='Any'}
} }
"#;
        let base=json!({"category":"Private","program":r"C:\RTXBridge fixture\sparkdev-link.exe","profile":2,"profileEnabled":"True","enabled":"True","action":"Block","direction":"Inbound","protocol":"UDP","localPort":"Any","bypass":false,"queryFails":false});
        let mut cases=vec![(base.clone(),true)];
        let mut public=base.clone();public["category"]=json!("Public");public["profile"]=json!(4);cases.push((public,true));
        let mut outbound=base.clone();outbound["direction"]=json!("Outbound");cases.push((outbound,true));
        for (key,value) in [
            ("profile",json!(4)),("program",json!(r"C:\Other program\sparkdev-link.exe")),
            ("action",json!("Allow")),("protocol",json!("TCP")),("localPort",json!("1234")),
            ("enabled",json!("False")),("profileEnabled",json!("False")),
            ("bypass",json!(true)),("queryFails",json!(true)),("category",json!("Unknown")),
        ] {
            let mut fixture=base.clone();fixture[key]=value;cases.push((fixture,false));
        }
        let powershell=Path::new(&std::env::var_os("SystemRoot").unwrap()).join("System32/WindowsPowerShell/v1.0/powershell.exe");
        for (fixture,known_block) in cases {
            let output=Command::new(&powershell)
                .args(["-NoProfile","-NonInteractive","-Command",&format!("{FIXTURE}\n{QUERY}")])
                .env("RTXBRIDGE_FIREWALL_FIXTURE",fixture.to_string())
                .env("RTXBRIDGE_LINK_BINARY",r"C:\RTXBridge fixture\sparkdev-link.exe")
                .env("RTXBRIDGE_LINK_TARGET","192.0.2.1")
                .stdin(Stdio::null()).creation_flags(0x08000000).output().unwrap();
            assert!(output.status.success());
            let observed:Observation=serde_json::from_slice(&output.stdout).unwrap_or_else(|_|panic!("Invalid fixture result for {fixture}"));
            assert_eq!(observed.known_block(),known_block,"{fixture}");
        }
    }
}
