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
//! `CIM_PowerManagementService.RequestPowerStateChange` for power, and
//! `CIM_BootConfigSetting.ChangeBootOrder` with
//! `CIM_BootService.SetBootConfigRole` for a one-time boot from the
//! network.

use crate::wsman::{self, Client, Selector};

const CIM: &str = "http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2";
const IPS: &str = "http://intel.com/wbem/wscim/1/ips-schema/1";

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
        /// Nonces handed out, and the highest count used with each.
        nonce: u32,
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
                let value = match wanted {
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
    fn a_wrong_password_says_so() {
        let fake = FakeAmt::start();
        let amt = Amt::new(&fake.address.to_string(), USER, "wrong");
        let err = amt.power_state().unwrap_err();
        assert!(err.contains("did not take the user and password"), "{err}");
    }
}
