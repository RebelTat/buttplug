// Buttplug Rust Source Code File - See https://buttplug.io for more info.
//
// Copyright 2016-2019 Nonpolynomial Labs LLC. All rights reserved.
//
// Licensed under the BSD 3-Clause license. See LICENSE file in the project root
// for full license information.

//! Communications API for accessing Buttplug Servers
pub mod client_event_loop;
mod client_message_sorter;
pub mod device;

use crate::{
  connector::{ButtplugConnector, ButtplugConnectorError, ButtplugConnectorFuture},
  core::{
    errors::{ButtplugError, ButtplugHandshakeError},
    messages::{
      ButtplugCurrentSpecClientMessage, ButtplugCurrentSpecServerMessage,
      ButtplugMessageSpecVersion, Ping, RequestDeviceList, RequestServerInfo, StartScanning,
      StopAllDevices, StopScanning,
    },
  },
  util::{
    async_manager,
    future::{ButtplugFuture, ButtplugFutureStateShared},
    stream::convert_broadcast_receiver_to_stream,
  },
};
#[cfg(feature = "server")]
use crate::server::ButtplugServer;
use client_event_loop::{ButtplugClientEventLoop, ButtplugClientRequest};
use dashmap::DashMap;
pub use device::{
  ButtplugClientDevice, ButtplugClientDeviceEvent, ButtplugClientDeviceMessageType, LinearCommand,
  RotateCommand, VibrateCommand,
};
use futures::{
  future::{self, BoxFuture},
  Stream,
};
use std::sync::{
  atomic::{AtomicBool, Ordering},
  Arc,
};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::{span::Span, Level};
use tracing_futures::Instrument;

/// Result type used for public APIs.
///
/// Allows us to differentiate between an issue with the connector (as a
/// [ButtplugConnectorError]) and an issue within Buttplug (as a
/// [ButtplugError]).
type ButtplugClientResult<T = ()> = Result<T, ButtplugClientError>;
type ButtplugClientResultFuture<T = ()> = BoxFuture<'static, ButtplugClientResult<T>>;

/// Result type used for passing server responses.
pub type ButtplugServerMessageResult = ButtplugClientResult<ButtplugCurrentSpecServerMessage>;
pub type ButtplugServerMessageResultFuture =
  ButtplugClientResultFuture<ButtplugCurrentSpecServerMessage>;
/// Future state type for returning server responses across futures.
pub(crate) type ButtplugServerMessageStateShared =
  ButtplugFutureStateShared<ButtplugServerMessageResult>;
/// Future type that expects server responses.
pub(crate) type ButtplugServerMessageFuture = ButtplugFuture<ButtplugServerMessageResult>;

/// Future state for messages sent from the client that expect a server
/// response.
///
/// When a message is sent from the client and expects a response from the
/// server, we'd like to know when that response arrives, and usually we'll want
/// to wait for it. We can do so by creating a future that will be resolved when
/// a response is received from the server.
///
/// To do this, we build a [ButtplugFuture], then take its waker and pass it
/// along with the message we send to the connector, using the
/// [ButtplugClientMessageFuturePair] type. We can then expect the connector to
/// get the response from the server, match it with our message (using something
/// like the
/// [ClientConnectorMessageSorter][crate::connector::ClientConnectorMessageSorter]),
/// and set the reply in the waker we've sent along. This will resolve the
/// future we're waiting on and allow us to continue execution.
#[derive(Clone)]
pub struct ButtplugClientMessageFuturePair {
  pub msg: ButtplugCurrentSpecClientMessage,
  pub waker: ButtplugServerMessageStateShared,
}

impl ButtplugClientMessageFuturePair {
  pub fn new(
    msg: ButtplugCurrentSpecClientMessage,
    waker: ButtplugServerMessageStateShared,
  ) -> Self {
    Self { msg, waker }
  }
}

/// Represents all of the different types of errors a ButtplugClient can return.
///
/// Clients can return two types of errors:
///
/// - [ButtplugConnectorError], which means there was a problem with the
/// connection between the client and the server, like a network connection
/// issue.
/// - [ButtplugError], which is an error specific to the Buttplug Protocol.
#[derive(Debug, Error)]
pub enum ButtplugClientError {
  /// Connector error
  #[error(transparent)]
  ButtplugConnectorError(#[from] ButtplugConnectorError),
  /// Protocol error
  #[error(transparent)]
  ButtplugError(#[from] ButtplugError),
}

/// Enum representing different events that can be emitted by a client.
///
/// These events are created by the server and sent to the client, and represent
/// unrequested actions that the client will need to respond to, or that
/// applications using the client may be interested in.
#[derive(Clone, Debug)]
pub enum ButtplugClientEvent {
  /// Emitted when a scanning session (started via a StartScanning call on
  /// [ButtplugClient]) has finished.
  ScanningFinished,
  /// Emitted when a device has been added to the server. Includes a
  /// [ButtplugClientDevice] object representing the device.
  DeviceAdded(Arc<ButtplugClientDevice>),
  /// Emitted when a device has been removed from the server. Includes a
  /// [ButtplugClientDevice] object representing the device.
  DeviceRemoved(Arc<ButtplugClientDevice>),
  /// Emitted when a client has not pinged the server in a sufficient amount of
  /// time.
  PingTimeout,
  /// Emitted when the client successfully connects to a server.
  ServerConnect,
  /// Emitted when a client connector detects that the server has disconnected.
  ServerDisconnect,
  /// Emitted when an error that cannot be matched to a request is received from
  /// the server.
  Error(ButtplugError),
}

impl Unpin for ButtplugClientEvent {}

/// Struct used by applications to communicate with a Buttplug Server.
///
/// Buttplug Clients provide an API layer on top of the Buttplug Protocol that
/// handles boring things like message creation and pairing, protocol ordering,
/// etc... This allows developers to concentrate on controlling hardware with
/// the API.
///
/// Clients serve a few different purposes:
/// - Managing connections to servers, thru [ButtplugClientConnector]s
/// - Emitting events received from the Server
/// - Holding state related to the server (i.e. what devices are currently
///   connected, etc...)
///
/// Clients are created by the [ButtplugClient::run()] method, which also
/// handles spinning up the event loop and connecting the client to the server.
/// Closures passed to the run() method can access and use the Client object.
pub struct ButtplugClient {
  /// The client name. Depending on the connection type and server being used,
  /// this name is sometimes shown on the server logs or GUI.
  client_name: String,
  /// The server name that we're current connected to.
  server_name: Arc<Mutex<Option<String>>>,
  event_stream: broadcast::Sender<ButtplugClientEvent>,
  // Sender to relay messages to the internal client loop
  message_sender: broadcast::Sender<ButtplugClientRequest>,
  connected: Arc<AtomicBool>,
  _client_span: Arc<Mutex<Option<Span>>>,
  device_map: Arc<DashMap<u32, Arc<ButtplugClientDevice>>>,
}

unsafe impl Send for ButtplugClient {}
// Not actually sure this should be sync, but trying to call handshake breaks
// without it.
unsafe impl Sync for ButtplugClient {}

impl ButtplugClient {
  pub fn new(name: &str) -> Self {
    let (message_sender, _) = broadcast::channel(256);
    let (event_stream, _) = broadcast::channel(256);
    Self {
      client_name: name.to_owned(),
      server_name: Arc::new(Mutex::new(None)),
      event_stream,
      message_sender,
      _client_span: Arc::new(Mutex::new(None)),
      connected: Arc::new(AtomicBool::new(false)),
      device_map: Arc::new(DashMap::new()),
    }
  }

  pub async fn connect<ConnectorType>(
    &self,
    mut connector: ConnectorType,
  ) -> Result<(), ButtplugClientError>
  where
    ConnectorType: ButtplugConnector<ButtplugCurrentSpecClientMessage, ButtplugCurrentSpecServerMessage>
      + 'static,
  {
    if self.connected() {
      return Err(ButtplugClientError::ButtplugConnectorError(
        ButtplugConnectorError::ConnectorAlreadyConnected,
      ));
    }

    // TODO I cannot remember why this is here or what it does.
    *self._client_span.lock().await = {
      let span = span!(Level::INFO, "Client");
      let _ = span.enter();
      Some(span)
    };
    info!("Connecting to server.");
    let (connector_sender, connector_receiver) = mpsc::channel(256);
    connector.connect(connector_sender).await.map_err(|e| {
      error!("Connection to server failed: {:?}", e);
      ButtplugClientError::from(e)
    })?;
    info!("Connection to server succeeded.");
    let mut client_event_loop = ButtplugClientEventLoop::new(
      self.connected.clone(),
      connector,
      connector_receiver,
      self.event_stream.clone(),
      self.message_sender.clone(),
      self.device_map.clone(),
    );

    // Start the event loop before we run the handshake.
    async_manager::spawn(
      async move {
        client_event_loop.run().await;
      }
      .instrument(tracing::info_span!("Client Loop Span")),
    )
    .unwrap();
    self.run_handshake().await
  }

  /// Convenience function for creating in-process connectors.
  ///
  /// Creates a [ButtplugClient] event loop, with an in-process connector with
  /// all device managers that ship with the library and work on the current
  /// platform added to it already. Takes a maximum ping time to build the
  /// server with, other parameters match `run()`.
  ///
  /// # When To Use This Instead of `run()`
  ///
  /// If you just want to build a quick example and save yourself a few use
  /// statements and setup, this will get you going. For anything *production*,
  /// we recommend using `run()` as you will have more control over what
  /// happens. This method may gain/lose device comm managers at any time.
  ///
  /// # The Device I Want To Use Doesn't Show Up
  ///
  /// If you are trying to use this method to create your client, and do not see
  /// the devices you want, there are a couple of things to check:
  ///
  /// - Are you on a platform that the device communication manager supports?
  ///   For instance, we only support XInput on windows.
  /// - Did the developers add a new Device CommunicationManager type and forget
  ///   to add it to this method? _It's more likely than you think!_ [File a
  ///   bug](https://github.com/buttplugio/buttplug-rs/issues).
  ///
  /// # Errors
  ///
  /// If the library was compiled without any device managers, the
  /// [ButtplugClient] will have nothing to do. This is considered a
  /// catastrophic failure and the library will return an error.
  ///
  /// If the library is using outside device managers, it is recommended to
  /// build your own connector, add your device manager to those, and use the
  /// `run()` method to pass it in.
  #[cfg(feature = "server")]
  pub async fn connect_in_process(
    &self,
    server: Option<ButtplugServer>
  ) -> Result<(), ButtplugClientError> {
    use crate::connector::ButtplugInProcessClientConnector;

    let connector = ButtplugInProcessClientConnector::new(server);
    #[cfg(feature = "btleplug-manager")]
    {
      use crate::server::comm_managers::btleplug::BtlePlugCommunicationManagerBuilder;
      connector
        .server_ref()
        .device_manager()
        .add_comm_manager(BtlePlugCommunicationManagerBuilder::default())
        .unwrap();
    }
    #[cfg(feature = "websocket-server-manager")]
    {
      use crate::server::comm_managers::websocket_server::websocket_server_comm_manager::WebsocketServerDeviceCommunicationManagerBuilder;
      connector
        .server_ref()
        .device_manager()
        .add_comm_manager(
          WebsocketServerDeviceCommunicationManagerBuilder::default().listen_on_all_interfaces(true),
        )
        .unwrap();
    }
    #[cfg(feature = "serial-manager")]
    {
      use crate::server::comm_managers::serialport::SerialPortCommunicationManagerBuilder;
      connector
        .server_ref()
        .device_manager()
        .add_comm_manager(SerialPortCommunicationManagerBuilder::default())
        .unwrap();
    }
    #[cfg(feature = "lovense-connect-service-manager")]
    {
      use crate::server::comm_managers::lovense_connect_service::LovenseConnectServiceCommunicationManagerBuilder;
      connector
        .server_ref()
        .device_manager()
        .add_comm_manager(LovenseConnectServiceCommunicationManagerBuilder::default())
        .unwrap();
    }
    #[cfg(feature = "lovense-dongle-manager")]
    {
      use crate::server::comm_managers::lovense_dongle::{
        LovenseHIDDongleCommunicationManagerBuilder, LovenseSerialDongleCommunicationManagerBuilder,
      };
      connector
        .server_ref()
        .device_manager()
        .add_comm_manager(LovenseHIDDongleCommunicationManagerBuilder::default())
        .unwrap();
      connector
        .server_ref()
        .device_manager()
        .add_comm_manager(LovenseSerialDongleCommunicationManagerBuilder::default())
        .unwrap();
    }
    #[cfg(all(feature = "xinput-manager", target_os = "windows"))]
    {
      use crate::server::comm_managers::xinput::XInputDeviceCommunicationManagerBuilder;
      connector
        .server_ref()
        .device_manager()
        .add_comm_manager(XInputDeviceCommunicationManagerBuilder::default())
        .unwrap();
    }
    self.connect(connector).await
  }

  /// Creates the ButtplugClient instance and tries to establish a connection.
  ///
  /// Takes all of the components needed to build a [ButtplugClient], creates
  /// the struct, then tries to run connect and execute the Buttplug protocol
  /// handshake. Will return a connected and ready to use ButtplugClient is all
  /// goes well.
  async fn run_handshake(&self) -> ButtplugClientResult {
    // Run our handshake
    info!("Running handshake with server.");
    let msg = self
      .send_message_ignore_connect_status(
        RequestServerInfo::new(&self.client_name, ButtplugMessageSpecVersion::Version2).into(),
      )
      .await?;

    debug!("Got ServerInfo return.");
    if let ButtplugCurrentSpecServerMessage::ServerInfo(server_info) = msg {
      info!("Connected to {}", server_info.server_name());
      *self.server_name.lock().await = Some(server_info.server_name().clone());
      // Don't set ourselves as connected until after ServerInfo has been
      // received. This means we avoid possible races with the RequestServerInfo
      // handshake.
      self.connected.store(true, Ordering::SeqCst);

      // Get currently connected devices. The event loop will
      // handle sending the message and getting the return, and
      // will send the client updates as events.
      let msg = self
        .send_message(RequestDeviceList::default().into())
        .await?;
      if let ButtplugCurrentSpecServerMessage::DeviceList(m) = msg {
        self
          .send_message_to_event_loop(ButtplugClientRequest::HandleDeviceList(m))
          .await?;
      }
      Ok(())
    } else {
      self.disconnect().await?;
      Err(ButtplugClientError::ButtplugError(
        ButtplugHandshakeError::UnexpectedHandshakeMessageReceived(format!("{:?}", msg)).into(),
      ))
    }
  }

  /// Returns true if client is currently connected.
  pub fn connected(&self) -> bool {
    self.connected.load(Ordering::SeqCst)
  }

  /// Disconnects from server, if connected.
  ///
  /// Returns Err(ButtplugClientError) if disconnection fails. It can be assumed
  /// that even on failure, the client will be disconnected.
  pub fn disconnect(&self) -> ButtplugClientResultFuture {
    if !self.connected() {
      return Box::pin(future::ready(Err(
        ButtplugConnectorError::ConnectorNotConnected.into(),
      )));
    }
    // Send the connector to the internal loop for management. Once we throw
    // the connector over, the internal loop will handle connecting and any
    // further communications with the server, if connection is successful.
    let fut = ButtplugConnectorFuture::default();
    let msg = ButtplugClientRequest::Disconnect(fut.get_state_clone());
    let send_fut = self.send_message_to_event_loop(msg);
    let connected = self.connected.clone();
    Box::pin(async move {
      send_fut.await?;
      connected.store(false, Ordering::SeqCst);
      Ok(())
    })
  }

  /// Tells server to start scanning for devices.
  ///
  /// Returns Err([ButtplugClientError]) if request fails due to issues with
  /// DeviceManagers on the server, disconnection, etc.
  pub fn start_scanning(&self) -> ButtplugClientResultFuture {
    self.send_message_expect_ok(StartScanning::default().into())
  }

  /// Tells server to stop scanning for devices.
  ///
  /// Returns Err([ButtplugClientError]) if request fails due to issues with
  /// DeviceManagers on the server, disconnection, etc.
  pub fn stop_scanning(&self) -> ButtplugClientResultFuture {
    self.send_message_expect_ok(StopScanning::default().into())
  }

  /// Tells server to stop all devices.
  ///
  /// Returns Err([ButtplugClientError]) if request fails due to issues with
  /// DeviceManagers on the server, disconnection, etc.
  pub fn stop_all_devices(&self) -> ButtplugClientResultFuture {
    self.send_message_expect_ok(StopAllDevices::default().into())
  }

  pub fn event_stream(&self) -> impl Stream<Item = ButtplugClientEvent> {
    let stream = convert_broadcast_receiver_to_stream(self.event_stream.subscribe());
    // We can either Box::pin here or force the user to pin_mut!() on their
    // end. While this does end up with a dynamic dispatch on our end, it
    // still makes the API nicer for the user, so we'll just eat the perf hit.
    // Not to mention, this is not a high throughput system really, so it
    // shouldn't matter.
    Box::pin(stream)
  }

  /// Send message to the internal event loop.
  ///
  /// Mostly for handling boilerplate around possible send errors.
  fn send_message_to_event_loop(
    &self,
    msg: ButtplugClientRequest,
  ) -> BoxFuture<'static, Result<(), ButtplugClientError>> {
    // If we're running the event loop, we should have a message_sender.
    // Being connected to the server doesn't matter here yet because we use
    // this function in order to connect also.
    //
    // The message sender doesn't require an async send now, but we still want
    // to delay execution as part of our future in order to keep task coherency.
    let message_sender = self.message_sender.clone();
    Box::pin(async move {
      message_sender
        .send(msg)
        .map_err(|_| ButtplugConnectorError::ConnectorChannelClosed)?;
      Ok(())
    })
  }

  fn send_message(
    &self,
    msg: ButtplugCurrentSpecClientMessage,
  ) -> ButtplugServerMessageResultFuture {
    if !self.connected() {
      Box::pin(future::ready(Err(
        ButtplugConnectorError::ConnectorNotConnected.into(),
      )))
    } else {
      self.send_message_ignore_connect_status(msg)
    }
  }

  /// Sends a ButtplugMessage from client to server. Expects to receive a
  /// ButtplugMessage back from the server.
  fn send_message_ignore_connect_status(
    &self,
    msg: ButtplugCurrentSpecClientMessage,
  ) -> ButtplugServerMessageResultFuture {
    // Create a future to pair with the message being resolved.
    let fut = ButtplugServerMessageFuture::default();
    let internal_msg = ButtplugClientRequest::Message(ButtplugClientMessageFuturePair::new(
      msg,
      fut.get_state_clone(),
    ));

    // Send message to internal loop and wait for return.
    let send_fut = self.send_message_to_event_loop(internal_msg);
    Box::pin(async move {
      send_fut.await?;
      fut.await
    })
  }

  /// Sends a ButtplugMessage from client to server. Expects to receive an [Ok]
  /// type ButtplugMessage back from the server.
  fn send_message_expect_ok(
    &self,
    msg: ButtplugCurrentSpecClientMessage,
  ) -> ButtplugClientResultFuture {
    let send_fut = self.send_message(msg);
    Box::pin(async move { send_fut.await.map(|_| ()).map_err(|err| err) })
  }

  /// Retreives a list of currently connected devices.
  pub fn devices(&self) -> Vec<Arc<ButtplugClientDevice>> {
    self
      .device_map
      .iter()
      .map(|map_pair| map_pair.value().clone())
      .collect()
  }

  pub fn ping(&self) -> ButtplugClientResultFuture {
    let ping_fut = self.send_message_expect_ok(Ping::default().into());
    Box::pin(async move { ping_fut.await })
  }

  pub fn server_name(&self) -> Option<String> {
    // We'd have to be calling server_name in an extremely tight, asynchronous
    // loop for this to return None, so we'll treat this as lockless.
    //
    // Dear users actually reading this code: This is not an invitation for you
    // to get the server name in a tight, asynchronous loop. This will never
    // change throughout the life to the connection.
    if let Ok(name) = self.server_name.try_lock() {
      name.clone()
    } else {
      None
    }
  }
}
