use buttplug::core::connector::ButtplugConnectorResult;
use buttplug::core::message::{ActuatorType, ClientDeviceMessageAttributes};
use buttplug::core::{
    connector::{ButtplugConnector, ButtplugConnectorError},
    message::*,
};
use buttplug::server::device::configuration::{
    ServerDeviceMessageAttributesBuilder, ServerGenericDeviceMessageAttributes,
};
use buttplug::{
    core::message::{self, ButtplugMessage, DeviceList},
    core::message::{ButtplugMessageSpecVersion, ServerInfo},
    util::async_manager,
};

use tokio::sync::mpsc::channel;
use tokio::{sync::mpsc::Sender, time::sleep};

use serde::Serialize;
use serde_json::{self, Value};

use futures::{future::BoxFuture, FutureExt};
use std::ops::{DerefMut, RangeInclusive};
use std::sync::Mutex;
use std::time::Duration;
use std::{cmp, vec};
use std::{collections::HashMap, sync::Arc};
use tracing::{debug, error, warn};

use std::time::Instant;

#[derive(Clone)]
pub struct FakeConnectorCallRegistry {
    pub actions: Arc<Mutex<HashMap<u32, Box<Vec<FakeMessage>>>>>,
}

#[derive(Clone, Debug)]
pub struct FakeMessage {
    pub message: ButtplugCurrentSpecClientMessage,
    pub time: Instant,
}

#[allow(dead_code)]
impl FakeMessage {
    pub fn new(msg: ButtplugCurrentSpecClientMessage) -> Self {
        FakeMessage {
            message: msg,
            time: Instant::now(),
        }
    }

    pub fn assert_strenth(&self, strength: f64) -> &Self {
        match self.message.clone() {
            message::ButtplugSpecV3ClientMessage::ScalarCmd(cmd) => {
                cmd.scalars().iter().all(|v| {
                    let actual = v.scalar();
                    assert_eq!(strength, actual);
                    true
                });
            }
            _ => panic!("Message is not scalar cmd"),
        }
        self
    }

    pub fn assert_strengths(&self, strengths: Vec<(u32, f64)>) -> &Self {
        match self.message.clone() {
            message::ButtplugSpecV3ClientMessage::ScalarCmd(cmd) => {
                for (index, expected) in strengths.iter() {
                    let sub_cmd: &ScalarSubcommand = cmd
                        .scalars()
                        .iter()
                        .filter(|x| &x.index() == index)
                        .next()
                        .unwrap();
                    assert_eq!(expected, &sub_cmd.scalar(), "actuator #{}", index);
                    assert_eq!(strengths.len(), cmd.scalars().len(), "same amonut of calls")
                }
            }
            _ => panic!("Message is not scalar cmd"),
        }
        self
    }

    pub fn assert_position(&self, position: f64) -> &Self {
        match self.message.clone() {
            message::ButtplugSpecV3ClientMessage::LinearCmd(cmd) => {
                cmd.vectors().iter().all(|v| {
                    let actual: f64 = v.position();
                    assert_eq!(position, actual);
                    true
                });
            }
            _ => panic!("Message is not linear cmd"),
        }
        self
    }

    pub fn assert_duration(&self, duration: u32) -> &Self {
        match self.message.clone() {
            message::ButtplugSpecV3ClientMessage::LinearCmd(cmd) => {
                cmd.vectors().iter().all(|v| {
                    let actual = v.duration();
                    assert!(
                        actual > duration - 10 && actual < duration + 10,
                        "{}ms is not {}ms +/-10",
                        actual,
                        duration
                    );
                    true
                });
            }
            _ => panic!("Message is not linear cmd"),
        }
        self
    }

    pub fn assert_timestamp(&self, time_ms: i32, start_instant: Instant) -> &Self {
        debug!("self.time.elapsed: {:?}", self.time.elapsed());
        debug!("start_instant.elapsed: {:?}", start_instant.elapsed());
        let elapsed_ms = (start_instant.elapsed() - self.time.elapsed()).as_millis() as i32;
        assert!(
            elapsed_ms > time_ms - 25 && elapsed_ms < time_ms + 25,
            "Elapsed {}ms != timestamp {}ms +/-25",
            elapsed_ms,
            time_ms
        );
        self
    }

    pub fn assert_rotation(&self, strength: f64) -> &Self {
        match self.message.clone() {
            message::ButtplugSpecV3ClientMessage::RotateCmd(cmd) => {
                cmd.rotations().iter().all(|v| {
                    let actual = v.speed();
                    assert_eq!(strength, actual);
                    true
                });
            }
            _ => panic!("Message is not rotation cmd"),
        }
        self
    }

    pub fn assert_direction(&self, clockwise: bool) -> &Self {
        match self.message.clone() {
            message::ButtplugSpecV3ClientMessage::RotateCmd(cmd) => {
                cmd.rotations().iter().all(|v| {
                    let actual = v.clockwise();
                    assert_eq!(actual, clockwise);
                    true
                });
            }
            _ => panic!("Message is not rotation cmd"),
        }
        self
    }

    pub fn vibration_stopped(&self) -> bool {
        match self.message.clone() {
            message::ButtplugSpecV3ClientMessage::ScalarCmd(cmd) => {
                cmd.scalars().iter().all(|v| v.scalar() == 0.0)
            }
            _ => panic!("Message is not scalar cmd"),
        }
    }
}

#[allow(dead_code)]
impl FakeConnectorCallRegistry {
    fn default() -> Self {
        Self {
            actions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn store_record<T>(&self, imp: &T, cmd: FakeMessage)
    where
        T: Serialize,
    {
        let mut calls = self.actions.try_lock().unwrap();
        let device_id = get_value(imp, "DeviceIndex").parse().unwrap();
        let mut bucket = match calls.get(&device_id) {
            Some(some) => some.clone(),
            None => Box::new(vec![]),
        };
        bucket.deref_mut().push(cmd);
        calls.deref_mut().insert(device_id, bucket);
    }

    pub fn get_device(&self, device_id: u32) -> Vec<FakeMessage> {
        match self.actions.lock().unwrap().get(&device_id) {
            Some(some) => *some.clone(),
            None => vec![],
        }
    }

    pub fn assert_unused(&self, device_id: u32) {
        assert_eq!(self.get_device(device_id).len(), 0);
    }
}

pub struct FakeDeviceConnector {
    pub devices: Vec<DeviceAdded>,
    server_outbound_sender: Sender<ButtplugCurrentSpecServerMessage>,
    call_registry: FakeConnectorCallRegistry,
}

// Connector that allows to instantiate various fake devices for testing purposes
#[allow(dead_code)]
impl FakeDeviceConnector {
    pub fn new(devices: Vec<DeviceAdded>) -> (Self, FakeConnectorCallRegistry) {
        let (server_outbound_sender, _) = channel(256);
        let connector = FakeDeviceConnector {
            devices: devices,
            server_outbound_sender: server_outbound_sender,
            call_registry: FakeConnectorCallRegistry::default(),
        };
        let calls = connector.get_call_registry();
        (connector, calls)
    }

    pub fn device_demo() -> (Self, FakeConnectorCallRegistry) {
        Self::new(vec![
            vibrator(1, "Vibator 1"),
            vibrator(2, "Vibrator 2"),
            vibrator(3, "Vibrator 3"),
            linear(4, "Linear 1"),
            linear(5, "Linear 2"),
            linear(6, "Linear 3"),
            rotate(7, "Rotator 1"),
        ])
    }

    pub fn get_call_registry(&self) -> FakeConnectorCallRegistry {
        self.call_registry.clone()
    }

    fn ok_response(&self, msg_id: u32) -> buttplug::core::connector::ButtplugConnectorResultFuture {
        let sender = self.server_outbound_sender.clone();
        async move {
            let mut response = ButtplugSpecV3ServerMessage::Ok(message::Ok::default());
            response.set_id(msg_id);
            sender
                .send(response)
                .await
                .map_err(|_| ButtplugConnectorError::ConnectorNotConnected)
        }
        .boxed()
    }
}

impl ButtplugConnector<ButtplugCurrentSpecClientMessage, ButtplugCurrentSpecServerMessage>
    for FakeDeviceConnector
{
    fn connect(
        &mut self,
        message_sender: tokio::sync::mpsc::Sender<ButtplugCurrentSpecServerMessage>,
    ) -> BoxFuture<'static, Result<(), ButtplugConnectorError>> {
        let devices = self.devices.clone();
        let send = message_sender.clone();
        self.server_outbound_sender = message_sender.clone();
        async move {
            async_manager::spawn(async move {
                // assure that other thread has registered listener when the test devices
                // are added. Quick and dirty but its just test code anyways
                sleep(Duration::from_millis(10)).await;
                for device in devices {
                    if send
                        .send(ButtplugSpecV3ServerMessage::DeviceAdded(device))
                        .await
                        .is_err()
                    {
                        panic!();
                    }
                }
            });
            Ok(())
        }
        .boxed()
    }

    fn disconnect(&self) -> buttplug::core::connector::ButtplugConnectorResultFuture {
        async move { ButtplugConnectorResult::Ok(()) }.boxed()
    }

    fn send(
        &self,
        msg: ButtplugCurrentSpecClientMessage,
    ) -> buttplug::core::connector::ButtplugConnectorResultFuture {
        let msg_id = msg.id();
        let msg_clone = msg.clone();
        let sender = self.server_outbound_sender.clone();
        match msg {
            ButtplugCurrentSpecClientMessage::RequestServerInfo(_) => async move {
                sender
                    .send(ButtplugSpecV3ServerMessage::ServerInfo(ServerInfo::new(
                        "test server",
                        ButtplugMessageSpecVersion::Version3,
                        0,
                    )))
                    .await
                    .map_err(|_| ButtplugConnectorError::ConnectorNotConnected)
            }
            .boxed(),
            ButtplugCurrentSpecClientMessage::RequestDeviceList(_) => async move {
                let mut response: ButtplugSpecV3ServerMessage =
                    ButtplugSpecV3ServerMessage::DeviceList(DeviceList::new(vec![]));
                response.set_id(msg_id);
                sender
                    .send(response)
                    .await
                    .map_err(|_| ButtplugConnectorError::ConnectorNotConnected)
            }
            .boxed(),
            ButtplugCurrentSpecClientMessage::ScalarCmd(cmd) => {
                self.call_registry
                    .store_record(&cmd, FakeMessage::new(msg_clone));
                self.ok_response(msg_id)
            }
            ButtplugCurrentSpecClientMessage::LinearCmd(cmd) => {
                self.call_registry
                    .store_record(&cmd, FakeMessage::new(msg_clone));
                self.ok_response(msg_id)
            }
            ButtplugCurrentSpecClientMessage::RotateCmd(cmd) => {
                self.call_registry
                    .store_record(&cmd, FakeMessage::new(msg_clone));
                self.ok_response(msg_id)
            }
            ButtplugCurrentSpecClientMessage::StopAllDevices(_) => {
                // cannot store cause no id
                self.ok_response(msg_id)
            }
            ButtplugCurrentSpecClientMessage::StartScanning(cmd) => {
                self.call_registry
                    .store_record(&cmd, FakeMessage::new(msg_clone));
                self.ok_response(msg_id)
            }
            ButtplugCurrentSpecClientMessage::StopScanning(cmd) => {
                self.call_registry
                    .store_record(&cmd, FakeMessage::new(msg_clone));
                self.ok_response(msg_id)
            }
            _ => {
                error!("Unimplemented message type.");
                async move { ButtplugConnectorResult::Ok(()) }.boxed()
            }
        }
    }
}

fn get_value<T>(val: &T, key: &str) -> String
where
    T: Serialize,
{
    let value: Value = serde_json::from_str(&serde_json::to_string(val).unwrap()).unwrap();
    value[key].to_string().parse().unwrap()
}

#[allow(dead_code)]
pub fn vibrator(id: u32, name: &str) -> DeviceAdded {
    let attributes = ServerDeviceMessageAttributesBuilder::default()
        .scalar_cmd(&vec![ServerGenericDeviceMessageAttributes::new(
            &format!("Vibrator {}", id),
            &RangeInclusive::new(0, 10),
            ActuatorType::Vibrate,
        )])
        .finish();
    DeviceAdded::new(
        id,
        name,
        &None,
        &None,
        &ClientDeviceMessageAttributes::from(attributes),
    )
}

#[allow(dead_code)]
pub fn scalar(id: u32, name: &str, actuator: ActuatorType) -> DeviceAdded {
    scalars(id, name, actuator, 1)
}

#[allow(dead_code)]
pub fn scalars(id: u32, name: &str, actuator: ActuatorType, count: i32) -> DeviceAdded {
    let mut messages = vec![];
    for _ in 0..count {
        messages.push(ServerGenericDeviceMessageAttributes::new(
            &format!("Scalar {}", id),
            &RangeInclusive::new(0, 10),
            actuator,
        ))
    }
    let attributes = ServerDeviceMessageAttributesBuilder::default()
        .scalar_cmd(&messages)
        .finish();
    DeviceAdded::new(
        id,
        name,
        &None,
        &None,
        &ClientDeviceMessageAttributes::from(attributes),
    )
}

#[allow(dead_code)]
pub fn linear(id: u32, name: &str) -> DeviceAdded {
    let attributes = ServerDeviceMessageAttributesBuilder::default()
        .linear_cmd(&vec![ServerGenericDeviceMessageAttributes::new(
            &format!("Position {}", id),
            &RangeInclusive::new(0, 10),
            ActuatorType::Position,
        )])
        .finish();
    DeviceAdded::new(
        id,
        name,
        &None,
        &None,
        &ClientDeviceMessageAttributes::from(attributes),
    )
}

#[allow(dead_code)]
pub fn rotate(id: u32, name: &str) -> DeviceAdded {
    let attributes = ServerDeviceMessageAttributesBuilder::default()
        .rotate_cmd(&vec![ServerGenericDeviceMessageAttributes::new(
            &format!("Rotator {}", id),
            &RangeInclusive::new(0, 10),
            ActuatorType::Rotate,
        )])
        .finish();
    DeviceAdded::new(
        id,
        name,
        &None,
        &None,
        &ClientDeviceMessageAttributes::from(attributes),
    )
}

#[cfg(test)]
pub mod tests {
    pub struct ButtplugTestClient {
        pub client: ButtplugClient,
        pub call_registry: FakeConnectorCallRegistry,
        pub created_devices: Vec<Arc<ButtplugClientDevice>>,
    }

    impl ButtplugTestClient {
        pub fn get_device(&self, device_id: u32) -> Arc<ButtplugClientDevice> {
            self.created_devices
                .iter()
                .filter(|d| d.index() == device_id)
                .next()
                .unwrap()
                .clone()
        }

        pub fn get_device_calls(&self, device_id: u32) -> Vec<FakeMessage> {
            self.call_registry.get_device(device_id)
        }

        pub fn print_device_calls(&self, test_start: Instant) {
            for device in &self.created_devices {
                println!("Device: {}", device.index());
                let call_registry = &self.call_registry;
                for i in 0..call_registry.get_device(device.index()).len() {
                    let fake_call: FakeMessage =
                        call_registry.get_device(device.index())[i].clone();

                    let s = self.get_value(&fake_call);
                    let t = (test_start.elapsed() - fake_call.time.elapsed()).as_millis();
                    let perc = (s * 100.0).round();
                    println!(
                        " {:02} @{:04} ms {percent:>3}% {empty:=>width$}",
                        i,
                        t,
                        percent = perc as i32,
                        empty = "",
                        width = (perc / 5.0).floor() as usize
                    );
                }
                println!();
            }
        }

        fn get_value(&self, fake: &FakeMessage) -> f64 {
            match fake.message.clone() {
                message::ButtplugSpecV3ClientMessage::ScalarCmd(cmd) => {
                    cmd.scalars().iter().next().unwrap().scalar()
                }
                message::ButtplugSpecV3ClientMessage::LinearCmd(cmd) => {
                    cmd.vectors().iter().next().unwrap().position()
                }
                _ => panic!("Message is not supported"),
            }
        }
    }

    pub async fn get_test_client(devices: Vec<DeviceAdded>) -> ButtplugTestClient {
        let (connector, call_registry) = FakeDeviceConnector::new(devices);
        let client = ButtplugClient::new("FakeClient");
        client.connect(connector).await.unwrap();

        let _ = client.event_stream().next().await.unwrap();

        let devices = client.devices().clone();
        ButtplugTestClient {
            client: client,
            call_registry,
            created_devices: devices,
        }
    }

    use buttplug::{
        client::{
            ButtplugClient, ButtplugClientDevice, LinearCommand, RotateCommand, ScalarCommand,
        },
        core::message::ActuatorType,
    };
    use futures::StreamExt;
    use tracing::Level;

    use super::*;

    #[allow(dead_code)]
    fn enable_log() {
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .with_max_level(Level::DEBUG)
                .finish(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn adding_test_devices_works() {
        let client = get_test_client(vec![
            vibrator(1, "eins"),
            vibrator(2, "zwei"),
            vibrator(3, "drei"),
        ])
        .await;

        assert_eq!(
            client
                .created_devices
                .iter()
                .filter(|d| d.index() == 1)
                .next()
                .unwrap()
                .name(),
            "eins"
        );
        assert_eq!(
            client
                .created_devices
                .iter()
                .filter(|d| d.index() == 2)
                .next()
                .unwrap()
                .name(),
            "zwei"
        );
        assert_eq!(
            client
                .created_devices
                .iter()
                .filter(|d| d.index() == 3)
                .next()
                .unwrap()
                .name(),
            "drei"
        );
    }

    #[tokio::test]
    async fn call_registry_stores_vibrate() {
        // arrange
        let client: ButtplugTestClient = get_test_client(vec![vibrator(1, "vibrator")]).await;

        // act
        let device = &client.created_devices[0];
        let _ = device
            .scalar(&ScalarCommand::Scalar((1.0, ActuatorType::Vibrate)))
            .await;

        // asssert
        client.get_device_calls(1)[0].assert_strenth(1.0);
    }

    #[tokio::test]
    async fn call_registry_stores_linear() {
        // arrange
        let client: ButtplugTestClient = get_test_client(vec![linear(1, "linear")]).await;

        // act
        let device = &client.created_devices[0];
        let _ = device.linear(&LinearCommand::Linear(42, 0.9)).await;

        // asert
        client.get_device_calls(1)[0]
            .assert_position(0.9)
            .assert_duration(42);
    }

    #[tokio::test]
    async fn call_registry_stores_rotate() {
        // arrange
        let client: ButtplugTestClient = get_test_client(vec![rotate(1, "rotator")]).await;

        // act
        let device = &client.created_devices[0];
        let _ = device.rotate(&RotateCommand::Rotate(0.42, false)).await;

        // asert
        client.get_device_calls(1)[0]
            .assert_rotation(0.42)
            .assert_direction(false);
    }
}
