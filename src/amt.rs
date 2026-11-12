// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Intel AMT, through its WS-Management interface: a real machine's
//! power, its next boot, and what `dukovo-lab machines check` reports.
//!
//! AMT runs in the machine's management engine, beside whatever the
//! machine runs, and answers on the machine's own network port. It needs
//! setting up once in the machine's MEBx menu (Ctrl+P as it starts):
//! a password, an address on the lab network, and Admin Control Mode, so
//! nothing it does waits for someone at the machine to agree.
//!
//! The classes and methods are AMT's, as Intel's AMT SDK documents them:
//! `CIM_PowerManagementService.RequestPowerStateChange` for power,
//! `CIM_BootConfigSetting.ChangeBootOrder` with
//! `CIM_BootService.SetBootConfigRole` for a one-time boot from the
//! network, and for the remote screen (KVM) on the standard VNC port,
//! `IPS_KVMRedirectionSettingData`, `AMT_RedirectionService`'s listener and
//! `CIM_KVMRedirectionSAP.RequestStateChange`.

use crate::wsman::{self, Client, Selector};

const CIM: &str = "http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2";
const IPS: &str = "http://intel.com/wbem/wscim/1/ips-schema/1";
const AMT: &str = "http://intel.com/wbem/wscim/1/amt-schema/1";

/// The keys of AMT's services, which a Put or a method names them by.
const SERVICE_KEYS: [&str; 4] =
    ["CreationClassName", "Name", "SystemCreationClassName", "SystemName"];

/// What the machine's power is to become.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerChange {
    /// On.
    On,
    /// Off at once, as if the power were cut: the system gets no say.
    HardOff,
    /// The reset line: the machine restarts at once.
    Reset,
    /// Off, asked of the system as the power button asks it, so it shuts
    /// down in order.
    SoftOff,
}

impl PowerChange {
    /// The CIM power state AMT is asked for.
    fn state(self) -> u32 {
        match self {
            PowerChange::On => 2,
            PowerChange::HardOff => 8,
            PowerChange::Reset => 10,
            PowerChange::SoftOff => 12,
        }
    }
}

/// Whether a CIM power state AMT reports is on: anything but the off
/// states (soft off, hard off, hibernate) and their graceful kinds.
pub fn is_on(state: u32) -> bool {
    !matches!(state, 6 | 7 | 8 | 12 | 13)
}

/// What a CIM power state means, for messages.
pub fn describe(state: u32) -> String {
    let name = match state {
        2 => "on",
        3 => "light sleep",
        4 => "deep sleep",
        6 => "off (hard)",
        7 => "hibernating",
        8 => "off",
        12 => "off (graceful)",
        13 => "off (hard, graceful)",
        _ => "another state",
    };
    format!("{name} ({state})")
}

/// AMT's remote screen, as `machines check` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteScreen {
    /// Whether KVM is on in MEBx: nothing on the network turns it on
    /// otherwise.
    pub in_mebx: bool,
    /// Whether AMT asks someone at the machine for a code before each
    /// session: `IPS_OptInService`'s OptInRequired, 0 for never.
    pub consent: u32,
}

/// A password for AMT's remote screen, made afresh for each run: VNC's
/// passwords are eight characters, and AMT wants an upper-case letter, a
/// lower-case one, a digit and a mark among them.
pub fn screen_password() -> Result<String, String> {
    const UPPER: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ";
    const LOWER: &[u8] = b"abcdefghijkmnopqrstuvwxyz";
    const DIGITS: &[u8] = b"23456789";
    const MARKS: &[u8] = b"!#%*+-=?@^_";
    const ANY: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789!#%*+-=?@^_";
    let hex = crate::netboot::random_hex(8)?;
    let random: Vec<u8> =
        (0..8).map(|n| u8::from_str_radix(&hex[2 * n..2 * n + 2], 16).unwrap_or(0)).collect();
    let pick = |set: &[u8], byte: u8| char::from(set[usize::from(byte) % set.len()]);
    let sets = [UPPER, LOWER, DIGITS, MARKS, ANY, ANY, ANY, ANY];
    Ok(sets.iter().zip(random).map(|(set, byte)| pick(set, byte)).collect())
}

/// Sets `name` to `value` among `properties`, in its place, or last.
fn set(properties: &mut Vec<(String, String)>, name: &str, value: &str) {
    match properties.iter_mut().find(|(property, _)| property == name) {
        Some((_, old)) => *old = value.to_string(),
        None => properties.push((name.to_string(), value.to_string())),
    }
}

/// The value of `name` among `properties`.
fn value<'a>(properties: &'a [(String, String)], name: &str) -> Option<&'a str> {
    properties.iter().find(|(property, _)| property == name).map(|(_, value)| value.as_str())
}

/// The selectors that pick the instance `properties` describe, by `keys`.
fn selectors<'a>(properties: &'a [(String, String)], keys: &[&'a str]) -> Vec<Selector<'a>> {
    keys.iter().filter_map(|&key| value(properties, key).map(|value| (key, value))).collect()
}

/// AMT on one machine.
pub struct Amt {
    client: Client,
}

impl Amt {
    /// AMT at `address` (with a port, or the plain-HTTP one).
    pub fn new(address: &str, user: &str, password: &str) -> Amt {
        Amt { client: Client::new(address, user, password) }
    }

    /// The machine's power state, as a CIM power state.
    pub fn power_state(&self) -> Result<u32, String> {
        let items =
            self.client.enumerate(&format!("{CIM}/CIM_AssociatedPowerManagementService"))?;
        let state = wsman::text(&items, "PowerState").ok_or("AMT's answer gives no PowerState")?;
        state.parse().map_err(|_| format!("AMT gave the power state \"{state}\""))
    }

    /// Changes the machine's power.
    pub fn change_power(&self, change: PowerChange) -> Result<(), String> {
        let class = format!("{CIM}/CIM_PowerManagementService");
        let arguments = format!(
            "<h:RequestPowerStateChange_INPUT xmlns:h=\"{class}\">\
             <h:PowerState>{}</h:PowerState><h:ManagedElement>{}</h:ManagedElement>\
             </h:RequestPowerStateChange_INPUT>",
            change.state(),
            wsman::reference(
                &format!("{CIM}/CIM_ComputerSystem"),
                &[("CreationClassName", "CIM_ComputerSystem"), ("Name", "ManagedSystem")],
            ),
        );
        self.client
            .invoke(&class, "RequestPowerStateChange", &POWER_SERVICE, &arguments)
            .map(drop)
            .map_err(|err| format!("asking AMT for power state {}: {err}", change.state()))
    }

    /// Makes the machine's next boot, and only that one, from the
    /// network (PXE).
    pub fn boot_from_network_once(&self) -> Result<(), String> {
        let setting = format!("{CIM}/CIM_BootConfigSetting");
        let order = format!(
            "<h:ChangeBootOrder_INPUT xmlns:h=\"{setting}\"><h:Source>{}</h:Source>\
             </h:ChangeBootOrder_INPUT>",
            wsman::reference(
                &format!("{CIM}/CIM_BootSourceSetting"),
                &[("InstanceID", "Intel(r) AMT: Force PXE Boot")],
            ),
        );
        self.client
            .invoke(&setting, "ChangeBootOrder", &BOOT_SETTING, &order)
            .map_err(|err| format!("asking AMT to boot from the network next: {err}"))?;
        // Role 1, IsNextSingleUse: the order above is for the next boot.
        let service = format!("{CIM}/CIM_BootService");
        let role = format!(
            "<h:SetBootConfigRole_INPUT xmlns:h=\"{service}\"><h:BootConfigSetting>{}\
             </h:BootConfigSetting><h:Role>1</h:Role></h:SetBootConfigRole_INPUT>",
            wsman::reference(&setting, &BOOT_SETTING),
        );
        self.client
            .invoke(&service, "SetBootConfigRole", &BOOT_SERVICE, &role)
            .map(drop)
            .map_err(|err| format!("asking AMT to use that order for the next boot: {err}"))
    }

    /// AMT's firmware version.
    pub fn version(&self) -> Result<String, String> {
        let items = self.client.enumerate(&format!("{CIM}/CIM_SoftwareIdentity"))?;
        wsman::elements(&items, "CIM_SoftwareIdentity")
            .into_iter()
            .find(|item| wsman::text(item, "InstanceID").as_deref() == Some("AMT"))
            .and_then(|item| wsman::text(item, "VersionString"))
            .ok_or_else(|| "AMT did not say its version".into())
    }

    /// AMT's control mode: 1 is Client Control Mode, where remote screen
    /// sessions wait for someone at the machine to read out a code; 2 is
    /// Admin Control Mode, which the lab needs.
    pub fn control_mode(&self) -> Result<u32, String> {
        let answer = self.client.get(&format!("{IPS}/IPS_HostBasedSetupService"), &[])?;
        let mode =
            wsman::text(&answer, "CurrentControlMode").ok_or("AMT did not say its control mode")?;
        mode.parse().map_err(|_| format!("AMT gave the control mode \"{mode}\""))
    }

    /// The remote screen's state in MEBx, and whether it asks for consent.
    pub fn remote_screen(&self) -> Result<RemoteScreen, String> {
        let settings = self.client.get(&format!("{IPS}/IPS_KVMRedirectionSettingData"), &[])?;
        let in_mebx = wsman::text(&settings, "EnabledByMEBx").as_deref() != Some("false");
        let opt_in = self.client.get(&format!("{IPS}/IPS_OptInService"), &[])?;
        let consent = wsman::text(&opt_in, "OptInRequired").unwrap_or_default();
        let consent =
            consent.parse().map_err(|_| format!("AMT gave the consent setting \"{consent}\""))?;
        Ok(RemoteScreen { in_mebx, consent })
    }

    /// Turns AMT's remote screen on for a standard VNC client on port
    /// 5900, signing in with `password` (as [`screen_password`] makes):
    /// the port and its password, the redirection listener every remote
    /// session needs, and KVM itself. What is on already is left as it is.
    pub fn turn_on_remote_screen(&self, password: &str) -> Result<(), String> {
        let screen = self.remote_screen()?;
        if !screen.in_mebx {
            return Err("KVM is off in MEBx, and only MEBx turns it on: turn on KVM Feature \
                        Selection there (Ctrl+P as the machine starts)"
                .into());
        }
        if screen.consent != 0 {
            return Err(format!(
                "AMT asks someone at the machine for a code before each remote screen session \
                 (OptInRequired {}): set User Opt-in to None in MEBx",
                screen.consent
            ));
        }
        let kvm = format!("{IPS}/IPS_KVMRedirectionSettingData");
        let mut settings = self.instance(&kvm)?;
        set(&mut settings, "Is5900PortEnabled", "true");
        if value(&settings, "OptInPolicy").is_some() {
            set(&mut settings, "OptInPolicy", "false");
        }
        set(&mut settings, "RFBPassword", password);
        self.client
            .put(&kvm, &selectors(&settings, &["InstanceID"]), &wsman::instance(&kvm, &settings))
            .map_err(|err| format!("turning on port 5900 for the remote screen: {err}"))?;
        let redirection = format!("{AMT}/AMT_RedirectionService");
        let mut service = self.instance(&redirection)?;
        if value(&service, "ListenerEnabled") != Some("true") {
            set(&mut service, "ListenerEnabled", "true");
            self.client
                .put(
                    &redirection,
                    &selectors(&service, &SERVICE_KEYS),
                    &wsman::instance(&redirection, &service),
                )
                .map_err(|err| format!("turning on AMT's redirection listener: {err}"))?;
        }
        // KVM itself: 2 is on with a session, 6 on and waiting for one.
        let point = format!("{CIM}/CIM_KVMRedirectionSAP");
        let access = self.instance(&point)?;
        if !matches!(value(&access, "EnabledState"), Some("2" | "6")) {
            let arguments = format!(
                "<h:RequestStateChange_INPUT xmlns:h=\"{point}\">\
                 <h:RequestedState>2</h:RequestedState></h:RequestStateChange_INPUT>"
            );
            self.client
                .invoke(
                    &point,
                    "RequestStateChange",
                    &selectors(&access, &SERVICE_KEYS),
                    &arguments,
                )
                .map_err(|err| format!("turning on KVM: {err}"))?;
        }
        Ok(())
    }

    /// The properties of the one instance of `class`.
    fn instance(&self, class: &str) -> Result<Vec<(String, String)>, String> {
        let name = class.rsplit('/').next().unwrap_or(class);
        let answer = self.client.get(class, &[])?;
        let inside = wsman::elements(&answer, name)
            .first()
            .copied()
            .ok_or_else(|| format!("AMT's answer holds no {name}"))?;
        Ok(wsman::properties(inside))
    }

    /// The power states AMT can change the machine to.
    pub fn power_changes(&self) -> Result<Vec<u32>, String> {
        let items = self.client.enumerate(&format!("{CIM}/CIM_PowerManagementCapabilities"))?;
        Ok(wsman::texts(&items, "PowerStatesSupported")
            .into_iter()
            .filter_map(|state| state.parse().ok())
            .collect())
    }
}

/// AMT's power management service.
const POWER_SERVICE: [Selector<'static>; 4] = [
    ("CreationClassName", "CIM_PowerManagementService"),
    ("Name", "Intel(r) AMT Power Management Service"),
    ("SystemCreationClassName", "CIM_ComputerSystem"),
    ("SystemName", "Intel(r) AMT"),
];

/// AMT's boot service.
const BOOT_SERVICE: [Selector<'static>; 4] = [
    ("CreationClassName", "CIM_BootService"),
    ("Name", "Intel(r) AMT Boot Service"),
    ("SystemCreationClassName", "CIM_ComputerSystem"),
    ("SystemName", "Intel(r) AMT"),
];

/// AMT's one boot configuration.
const BOOT_SETTING: [Selector<'static>; 1] = [("InstanceID", "Intel(r) AMT: Boot Configuration 0")];

/// A pretend AMT for tests: the same messages and digest sign-in over
/// HTTP, and a machine whose power it changes.
#[cfg(test)]
pub mod fake {
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::hash::md5_hex;
    use crate::http::{self, Response};
    use crate::wsman;

    pub const USER: &str = "admin";
    pub const PASSWORD: &str = "Lab-Amt-1!";
    const REALM: &str = "Digest:0123456789ABCDEF0123456789ABCDEF";

    /// What happens at the pretend machine when AMT turns it on or resets
    /// it: told whether the boot is from the network.
    pub type OnBoot = Box<dyn Fn(bool) + Send>;

    /// Says whether a remote screen session is open.
    pub type Busy = Box<dyn Fn() -> bool + Send>;

    /// The pretend machine's state.
    pub struct State {
        pub power: u32,
        /// Set by ChangeBootOrder, used up by the next boot.
        pub network_next: bool,
        /// Whether the system shuts down when asked as by the power
        /// button, as one with AMT's agent would.
        pub graceful: bool,
        /// Each method invoked, by name.
        pub calls: Vec<String>,
        pub on_boot: Option<OnBoot>,
        pub screen: Screen,
        /// Whether a remote screen session is open: AMT turns power
        /// changes down then, with ReturnValue 2.
        pub screen_busy: Option<Busy>,
        /// Whether AMT answers: when not, it hangs up on each request.
        pub answering: bool,
        /// Nonces handed out, and the highest count used with each.
        nonce: u32,
    }

    /// The pretend remote screen's settings.
    pub struct Screen {
        pub in_mebx: bool,
        pub consent: u32,
        pub port_5900: bool,
        pub listener: bool,
        /// CIM_KVMRedirectionSAP's EnabledState: 3 off, 6 on.
        pub kvm: u32,
        pub password: Option<String>,
    }

    impl Screen {
        /// The password a VNC client signs in with, while the port is on.
        pub fn open_password(&self) -> Option<String> {
            let on = self.port_5900 && self.listener && matches!(self.kvm, 2 | 6);
            self.password.clone().filter(|_| on)
        }
    }

    /// Whether AMT would take `password` for the remote screen: eight
    /// characters, with an upper-case letter, a lower-case one, a digit
    /// and a mark.
    pub fn strong(password: &str) -> bool {
        password.len() == 8
            && password.bytes().any(|b| b.is_ascii_uppercase())
            && password.bytes().any(|b| b.is_ascii_lowercase())
            && password.bytes().any(|b| b.is_ascii_digit())
            && password.bytes().any(|b| b.is_ascii_punctuation())
    }

    /// A pretend AMT, listening until dropped.
    pub struct FakeAmt {
        pub address: SocketAddr,
        pub state: Arc<Mutex<State>>,
        done: Arc<AtomicBool>,
    }

    impl FakeAmt {
        pub fn start() -> FakeAmt {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let state = Arc::new(Mutex::new(State {
                power: 8,
                network_next: false,
                graceful: false,
                calls: Vec::new(),
                on_boot: None,
                screen_busy: None,
                answering: true,
                screen: Screen {
                    in_mebx: true,
                    consent: 0,
                    port_5900: false,
                    listener: false,
                    kvm: 3,
                    password: None,
                },
                nonce: 0,
            }));
            let done = Arc::new(AtomicBool::new(false));
            {
                let (state, done) = (Arc::clone(&state), Arc::clone(&done));
                std::thread::spawn(move || {
                    while !done.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((stream, _)) => answer(&stream, &state),
                            Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
                        }
                    }
                });
            }
            FakeAmt { address, state, done }
        }

        pub fn calls(&self) -> Vec<String> {
            self.state.lock().unwrap().calls.clone()
        }
    }

    impl Drop for FakeAmt {
        fn drop(&mut self) {
            self.done.store(true, Ordering::Relaxed);
        }
    }

    /// Whether the request signed in with the password, for the nonce
    /// handed out last.
    fn signed_in(request: &http::Request, nonce: u32) -> bool {
        let Some(header) = request.header("authorization") else { return false };
        let field = |name: &str| {
            header.split(", ").find_map(|part| {
                let part = part.trim().trim_start_matches("Digest ");
                let (key, value) = part.split_once('=')?;
                (key == name).then(|| value.trim_matches('"').to_string())
            })
        };
        let (Some(nc), Some(cnonce), Some(response), Some(said_nonce)) =
            (field("nc"), field("cnonce"), field("response"), field("nonce"))
        else {
            return false;
        };
        let ha1 = md5_hex(format!("{USER}:{REALM}:{PASSWORD}").as_bytes());
        let ha2 = md5_hex(b"POST:/wsman");
        let expected = md5_hex(format!("{ha1}:{said_nonce}:{nc}:{cnonce}:auth:{ha2}").as_bytes());
        said_nonce == format!("nonce{nonce}") && response == expected
    }

    fn answer(stream: &TcpStream, state: &Arc<Mutex<State>>) {
        let _ = stream.set_nonblocking(false);
        let Ok(request) = http::read_request(stream) else { return };
        let mut state_now = state.lock().unwrap();
        if !state_now.answering {
            return;
        }
        if request.path != "/wsman" || !signed_in(&request, state_now.nonce) {
            state_now.nonce += 1;
            let challenge = format!(
                "Digest realm=\"{REALM}\", nonce=\"nonce{}\", stale=\"false\", qop=\"auth\"",
                state_now.nonce
            );
            drop(state_now);
            http::write_response(
                stream,
                &Response::text(401, "sign in").with("WWW-Authenticate", challenge),
            );
            return;
        }
        let body = String::from_utf8_lossy(&request.body).into_owned();
        let action = wsman::text(&body, "Action").unwrap_or_default();
        let class = wsman::text(&body, "ResourceURI").unwrap_or_default();
        let method = action.rsplit('/').next().unwrap_or_default().to_string();
        let class_name = class.rsplit('/').next().unwrap_or_default().to_string();
        let inner = match method.as_str() {
            "Enumerate" => {
                format!("<g:EnumerateResponse><g:EnumerationContext>{class_name}</g:EnumerationContext></g:EnumerateResponse>")
            }
            "Pull" => {
                let item = match class_name.as_str() {
                    "CIM_AssociatedPowerManagementService" => format!(
                        "<h:{class_name}><h:PowerState>{}</h:PowerState></h:{class_name}>",
                        state_now.power
                    ),
                    "CIM_SoftwareIdentity" => "<h:CIM_SoftwareIdentity><h:InstanceID>Flash</h:InstanceID>\
                        <h:VersionString>11.8.50</h:VersionString></h:CIM_SoftwareIdentity>\
                        <h:CIM_SoftwareIdentity><h:InstanceID>AMT</h:InstanceID>\
                        <h:VersionString>11.8.50</h:VersionString></h:CIM_SoftwareIdentity>"
                        .into(),
                    "CIM_PowerManagementCapabilities" => format!(
                        "<h:{class_name}><h:PowerChangeCapabilities>3</h:PowerChangeCapabilities>\
                         <h:PowerStatesSupported>2</h:PowerStatesSupported>\
                         <h:PowerStatesSupported>8</h:PowerStatesSupported>\
                         <h:PowerStatesSupported>10</h:PowerStatesSupported>{}</h:{class_name}>",
                        if state_now.graceful { "<h:PowerStatesSupported>12</h:PowerStatesSupported>" } else { "" }
                    ),
                    _ => String::new(),
                };
                format!("<g:PullResponse><g:Items>{item}</g:Items><g:EndOfSequence/></g:PullResponse>")
            }
            "Get" if class_name == "IPS_HostBasedSetupService" => {
                "<h:IPS_HostBasedSetupService><h:CurrentControlMode>2</h:CurrentControlMode></h:IPS_HostBasedSetupService>".into()
            }
            "Get" if class_name == "IPS_KVMRedirectionSettingData" => format!(
                "<h:IPS_KVMRedirectionSettingData><h:ElementName>Intel(r) KVM Redirection Settings</h:ElementName>\
                 <h:InstanceID>Intel(r) KVM Redirection Settings</h:InstanceID>\
                 <h:EnabledByMEBx>{}</h:EnabledByMEBx><h:Is5900PortEnabled>{}</h:Is5900PortEnabled>\
                 <h:OptInPolicy>true</h:OptInPolicy><h:SessionTimeout>3</h:SessionTimeout>\
                 </h:IPS_KVMRedirectionSettingData>",
                state_now.screen.in_mebx, state_now.screen.port_5900
            ),
            "Get" if class_name == "IPS_OptInService" => format!(
                "<h:IPS_OptInService><h:OptInRequired>{}</h:OptInRequired></h:IPS_OptInService>",
                state_now.screen.consent
            ),
            "Get" if class_name == "AMT_RedirectionService" => format!(
                "<h:AMT_RedirectionService><h:CreationClassName>AMT_RedirectionService</h:CreationClassName>\
                 <h:ElementName>Intel(r) AMT Redirection Service</h:ElementName>\
                 <h:EnabledState>32771</h:EnabledState><h:ListenerEnabled>{}</h:ListenerEnabled>\
                 <h:Name>Intel(r) AMT Redirection Service</h:Name>\
                 <h:SystemCreationClassName>CIM_ComputerSystem</h:SystemCreationClassName>\
                 <h:SystemName>Intel(r) AMT</h:SystemName></h:AMT_RedirectionService>",
                state_now.screen.listener
            ),
            "Get" if class_name == "CIM_KVMRedirectionSAP" => format!(
                "<h:CIM_KVMRedirectionSAP><h:CreationClassName>CIM_KVMRedirectionSAP</h:CreationClassName>\
                 <h:EnabledState>{}</h:EnabledState><h:Name>KVM Redirection Service Access Point</h:Name>\
                 <h:SystemCreationClassName>CIM_ComputerSystem</h:SystemCreationClassName>\
                 <h:SystemName>ManagedSystem</h:SystemName></h:CIM_KVMRedirectionSAP>",
                state_now.screen.kvm
            ),
            // A Put must give the whole instance back, named by its keys.
            "Put" if class_name == "IPS_KVMRedirectionSettingData"
                && wsman::text(&body, "Selector").as_deref() == Some("Intel(r) KVM Redirection Settings")
                && wsman::text(&body, "ElementName").is_some()
                && wsman::text(&body, "RFBPassword").is_some_and(|password| strong(&password)) =>
            {
                state_now.screen.port_5900 = wsman::text(&body, "Is5900PortEnabled").as_deref() == Some("true");
                state_now.screen.password = wsman::text(&body, "RFBPassword");
                state_now.calls.push(format!("Put {class_name}"));
                "<h:IPS_KVMRedirectionSettingData/>".into()
            }
            "Put" if class_name == "AMT_RedirectionService"
                && wsman::texts(&body, "Selector").len() == 4
                && wsman::text(&body, "EnabledState").is_some() =>
            {
                state_now.screen.listener = wsman::text(&body, "ListenerEnabled").as_deref() == Some("true");
                state_now.calls.push(format!("Put {class_name}"));
                "<h:AMT_RedirectionService/>".into()
            }
            "RequestStateChange" if class_name == "CIM_KVMRedirectionSAP" => {
                let wanted = wsman::text(&body, "RequestedState").unwrap_or_default();
                state_now.calls.push(format!("{method} {wanted}"));
                if wanted == "2" && state_now.screen.in_mebx {
                    state_now.screen.kvm = 6;
                }
                "<h:RequestStateChange_OUTPUT><h:ReturnValue>0</h:ReturnValue></h:RequestStateChange_OUTPUT>".into()
            }
            "ChangeBootOrder" => {
                state_now.network_next = body.contains("Intel(r) AMT: Force PXE Boot");
                state_now.calls.push(method.clone());
                "<h:ChangeBootOrder_OUTPUT><h:ReturnValue>0</h:ReturnValue></h:ChangeBootOrder_OUTPUT>".into()
            }
            "SetBootConfigRole" => {
                state_now.calls.push(method.clone());
                "<h:SetBootConfigRole_OUTPUT><h:ReturnValue>0</h:ReturnValue></h:SetBootConfigRole_OUTPUT>".into()
            }
            "RequestPowerStateChange" => {
                let wanted: u32 = wsman::text(&body, "PowerState").unwrap_or_default().parse().unwrap_or(0);
                state_now.calls.push(format!("{method} {wanted}"));
                let on = super::is_on(state_now.power);
                // Asked with this lock let go: the screen's own lock is
                // taken before this one when a session opens.
                let busy = state_now.screen_busy.take();
                drop(state_now);
                let screen_open = busy.as_ref().is_some_and(|busy| busy());
                state_now = state.lock().unwrap();
                state_now.screen_busy = busy;
                let value = match wanted {
                    _ if screen_open => 2,
                    2 if on => 5,
                    10 if !on => 5,
                    12 if !state_now.graceful => 1,
                    8 | 12 => {
                        state_now.power = 8;
                        0
                    }
                    2 | 10 => {
                        state_now.power = 2;
                        let network = std::mem::take(&mut state_now.network_next);
                        let on_boot = state_now.on_boot.take();
                        drop(state_now);
                        if let Some(on_boot) = &on_boot {
                            on_boot(network);
                        }
                        state_now = state.lock().unwrap();
                        if state_now.on_boot.is_none() {
                            state_now.on_boot = on_boot;
                        }
                        0
                    }
                    _ => 2,
                };
                format!("<h:RequestPowerStateChange_OUTPUT><h:ReturnValue>{value}</h:ReturnValue></h:RequestPowerStateChange_OUTPUT>")
            }
            _ => {
                drop(state_now);
                http::write_response(
                    stream,
                    &Response::new(400, "application/soap+xml", "<a:Envelope><a:Body><a:Fault><a:Text>unknown</a:Text></a:Fault></a:Body></a:Envelope>"),
                );
                return;
            }
        };
        drop(state_now);
        let envelope = format!("<a:Envelope><a:Header/><a:Body>{inner}</a:Body></a:Envelope>");
        http::write_response(stream, &Response::new(200, "application/soap+xml", envelope));
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeAmt, PASSWORD, USER};
    use super::*;

    fn amt(fake: &FakeAmt) -> Amt {
        Amt::new(&fake.address.to_string(), USER, PASSWORD)
    }

    #[test]
    fn turns_the_machine_on_and_off() {
        let fake = FakeAmt::start();
        let amt = amt(&fake);
        assert_eq!(amt.power_state().unwrap(), 8);
        amt.change_power(PowerChange::On).unwrap();
        assert_eq!(amt.power_state().unwrap(), 2);
        // Graceful off needs the system's say, which this machine lacks.
        let err = amt.change_power(PowerChange::SoftOff).unwrap_err();
        assert!(err.contains("returned 1"), "{err}");
        amt.change_power(PowerChange::HardOff).unwrap();
        assert!(!is_on(amt.power_state().unwrap()));
        assert_eq!(
            fake.calls(),
            [
                "RequestPowerStateChange 2",
                "RequestPowerStateChange 12",
                "RequestPowerStateChange 8"
            ]
        );
    }

    #[test]
    fn boots_from_the_network_once() {
        let fake = FakeAmt::start();
        let booted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let booted = std::sync::Arc::clone(&booted);
            fake.state.lock().unwrap().on_boot =
                Some(Box::new(move |network| booted.lock().unwrap().push(network)));
        }
        let amt = amt(&fake);
        amt.boot_from_network_once().unwrap();
        amt.change_power(PowerChange::On).unwrap();
        amt.change_power(PowerChange::Reset).unwrap();
        assert_eq!(*booted.lock().unwrap(), [true, false]);
        assert_eq!(fake.calls()[..2], ["ChangeBootOrder", "SetBootConfigRole"]);
    }

    #[test]
    fn says_what_amt_is() {
        let fake = FakeAmt::start();
        let amt = amt(&fake);
        assert_eq!(amt.version().unwrap(), "11.8.50");
        assert_eq!(amt.control_mode().unwrap(), 2);
        assert_eq!(amt.power_changes().unwrap(), [2, 8, 10]);
    }

    #[test]
    fn turns_the_remote_screen_on() {
        let fake = FakeAmt::start();
        let amt = amt(&fake);
        assert_eq!(amt.remote_screen().unwrap(), RemoteScreen { in_mebx: true, consent: 0 });
        let password = screen_password().unwrap();
        amt.turn_on_remote_screen(&password).unwrap();
        assert_eq!(
            fake.calls(),
            [
                "Put IPS_KVMRedirectionSettingData",
                "Put AMT_RedirectionService",
                "RequestStateChange 2"
            ]
        );
        assert_eq!(fake.state.lock().unwrap().screen.open_password(), Some(password.clone()));
        // Once on, only the password changes.
        let again = screen_password().unwrap();
        amt.turn_on_remote_screen(&again).unwrap();
        assert_eq!(fake.calls().len(), 4);
        assert_eq!(fake.state.lock().unwrap().screen.open_password(), Some(again));
    }

    #[test]
    fn the_remote_screen_needs_mebx_and_no_consent() {
        let fake = FakeAmt::start();
        let amt = amt(&fake);
        fake.state.lock().unwrap().screen.consent = 1;
        let err = amt.turn_on_remote_screen("Lk7!pQ2#").unwrap_err();
        assert!(err.contains("User Opt-in to None"), "{err}");
        fake.state.lock().unwrap().screen.in_mebx = false;
        let err = amt.turn_on_remote_screen("Lk7!pQ2#").unwrap_err();
        assert!(err.contains("KVM Feature Selection"), "{err}");
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn screen_passwords_are_strong_and_new() {
        let passwords: Vec<String> = (0..50).map(|_| screen_password().unwrap()).collect();
        for password in &passwords {
            assert!(super::fake::strong(password), "{password}");
        }
        let mut unique = passwords.clone();
        unique.sort();
        unique.dedup();
        assert!(unique.len() > 45);
    }

    #[test]
    fn a_wrong_password_says_so() {
        let fake = FakeAmt::start();
        let amt = Amt::new(&fake.address.to_string(), USER, "wrong");
        let err = amt.power_state().unwrap_err();
        assert!(err.contains("did not take the user and password"), "{err}");
    }
}
