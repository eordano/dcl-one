use super::{
    CommonError,
    rpc_protocol::{
        RemoteError, Response, RpcMessageTypes, StreamMessage, fill_remote_error,
        parse::{
            ParseErrors, build_message_identifier, parse_message_identifier, parse_protocol_message,
        },
    },
    server::{ServerError, ServerInternalError, ServerResult, ServerResultError},
    service_module_definition::{
        BiStreamsResponse, ClientStreamsResponse, ServerStreamsResponse, UnaryResponse,
    },
    stream_protocol::Generator,
    transport::{Transport, TransportError},
};
use prost::Message;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{
    Mutex,
    mpsc::UnboundedSender,
    oneshot::{Receiver as OneShotReceiver, Sender as OneShotSender, channel as oneshot_channel},
};
use tracing::{debug, error};

use super::rpc_protocol::parse::parse_header;
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::info;

#[derive(Default)]
pub struct ServerMessagesHandler {
    pub streams_handler: Arc<StreamsHandler>,
    listeners: Mutex<HashMap<u32, UnboundedSender<StreamPackage>>>,
}

impl ServerMessagesHandler {
    pub fn new() -> Self {
        Self {
            streams_handler: Arc::new(StreamsHandler::new()),
            listeners: Mutex::new(HashMap::new()),
        }
    }

    /// Runs the handler in a spawned task, so requests are processed concurrently.
    pub fn process_unary_request<T: Transport + ?Sized + 'static>(
        &self,
        transport: Arc<T>,
        message_number: u32,
        procedure_handler: UnaryResponse,
    ) {
        tokio::spawn(async move {
            match procedure_handler.await {
                Ok(procedure_response) => {
                    let response = Response {
                        message_identifier: build_message_identifier(
                            RpcMessageTypes::Response as u32,
                            message_number,
                        ),
                        payload: procedure_response,
                    };

                    if let Err(err) = transport.send(response.encode_to_vec()).await {
                        error!(
                            "> ServerMessagesHandler > Error on sending procedure response through a transport - message num: {message_number} - error: {err:?}"
                        );
                        if !matches!(err, TransportError::Closed) {
                            send_remote_error(
                                transport,
                                message_number,
                                ServerError::UnexpectedErrorOnTransport.into(),
                            )
                            .await;
                        }
                    }
                }
                Err(procedure_remote_error) => {
                    send_remote_error(transport, message_number, procedure_remote_error).await;
                }
            };
        });
    }

    /// Runs the handler in a spawned task, so requests are processed concurrently.
    pub fn process_server_streams_request<T: Transport + ?Sized + 'static>(
        self: Arc<Self>,
        transport: Arc<T>,
        message_number: u32,
        port_id: u32,
        procedure_handler: ServerStreamsResponse,
    ) {
        tokio::spawn(async move {
            match self
                .open_server_stream(transport.clone(), message_number, port_id)
                .await
            {
                Ok(open_ack_listener) => match open_ack_listener.await {
                    Ok(_) => match procedure_handler.await {
                        Ok(generator) => {
                            if let Err(error) = self
                                .streams_handler
                                .send_streams_through_transport(
                                    transport.clone(),
                                    generator,
                                    port_id,
                                    message_number,
                                )
                                .await
                            {
                                error!(
                                    "> ServerMessagesHandler > process_server_streams_request > Error while executing StreamsHandler::send_streams_through_transport - Error: {error:?}"
                                );
                                if !matches!(error, CommonError::TransportWasClosed) {
                                    send_remote_error(
                                        transport,
                                        message_number,
                                        ServerError::UnexpectedErrorOnTransport.into(),
                                    )
                                    .await;
                                }
                            }
                        }
                        Err(procedure_remote_error) => {
                            send_remote_error(transport, message_number, procedure_remote_error)
                                .await
                        }
                    },
                    Err(e) => {
                        error!(
                            "> ServerMessagesHandler > process_server_streams_request > Error on receiving on a open_ack_listener, sender seems to be dropped: {e:?}"
                        );
                    }
                },
                Err(error) => {
                    error!(
                        "> ServerMessagesHandler > process_server_streams_request > Erron on opening a server stream: {error:?}"
                    );
                    if !matches!(
                        error,
                        ServerResultError::Internal(ServerInternalError::TransportWasClosed)
                    ) {
                        send_remote_error(
                            transport,
                            message_number,
                            ServerError::UnexpectedErrorOnTransport.into(),
                        )
                        .await;
                    }
                }
            }
        });
    }

    /// Runs the handler in a spawned task, so requests are processed concurrently.
    pub fn process_client_streams_request<T: Transport + ?Sized + 'static>(
        self: Arc<Self>,
        transport: Arc<T>,
        message_number: u32,
        client_stream_id: u32,
        procedure_handler: ClientStreamsResponse,
        listener: UnboundedSender<(RpcMessageTypes, u32, StreamMessage)>,
    ) {
        tokio::spawn(async move {
            self.register_listener(client_stream_id, listener).await;
            match procedure_handler.await {
                Ok(procedure_response) => {
                    self.send_response(transport, message_number, procedure_response)
                        .await;
                }
                Err(procedure_remote_err) => {
                    send_remote_error(transport, message_number, procedure_remote_err).await;
                }
            }
        });
    }

    /// Runs the handler in a spawned task, so requests are processed concurrently.
    pub fn process_bidir_streams_request<T: Transport + ?Sized + 'static>(
        self: Arc<Self>,
        transport: Arc<T>,
        message_number: u32,
        port_id: u32,
        client_stream_id: u32,
        listener: UnboundedSender<(RpcMessageTypes, u32, StreamMessage)>,
        procedure_handler: BiStreamsResponse,
    ) {
        tokio::spawn(async move {
            self.register_listener(client_stream_id, listener).await;
            match self
                .open_server_stream(transport.clone(), message_number, port_id)
                .await
            {
                Ok(open_ack_listener) => match open_ack_listener.await {
                    Ok(_) => match procedure_handler.await {
                        Ok(generator) => {
                            if let Err(err) = self
                                .streams_handler
                                .send_streams_through_transport(
                                    transport.clone(),
                                    generator,
                                    port_id,
                                    message_number,
                                )
                                .await
                            {
                                error!(
                                    "> ServerMessagesHandler > process_bidir_streams_request > Error while executing StreamsHandler::send_streams_through_transport - Error: {err:?}"
                                );
                                if !matches!(err, CommonError::TransportWasClosed) {
                                    send_remote_error(
                                        transport,
                                        message_number,
                                        ServerError::UnexpectedErrorOnTransport.into(),
                                    )
                                    .await;
                                }
                            }
                        }
                        Err(remote_error) => {
                            send_remote_error(transport, message_number, remote_error).await;
                        }
                    },
                    Err(_) => {
                        error!(
                            "> ServerMessagesHandler > process_bidir_streams_request > Error on receiving on a open_ack_listener, sender seems to be dropped"
                        );
                    }
                },
                Err(err) => {
                    if !matches!(
                        err,
                        ServerResultError::Internal(ServerInternalError::TransportWasClosed)
                    ) {
                        send_remote_error(
                            transport,
                            message_number,
                            ServerError::UnexpectedErrorOnTransport.into(),
                        )
                        .await;
                    }
                }
            }
        });
    }

    pub fn notify_new_client_stream(self: Arc<Self>, message_number: u32, payload: Vec<u8>) {
        tokio::spawn(async move {
            let lock = self.listeners.lock().await;
            let listener = lock.get(&message_number);
            if let Some(listener) = listener {
                if let Ok(stream_message) = StreamMessage::decode(payload.as_slice()) {
                    if listener
                        .send((
                            RpcMessageTypes::StreamMessage,
                            message_number,
                            stream_message,
                        ))
                        .is_err()
                    {
                        error!(
                            "> ServerMessagesHandler > notify_new_client_stream > Error while sending through the listener, channel seems to be closed "
                        );
                    }
                } else {
                    error!(
                        "> ServerMessagesHandler > notify_new_client_stream > Error while decoding payload into StreamMessage, something is corrupted or bad implemented"
                    );
                }
            }
        });
    }

    /// Retries until [`Transport::send`] reports [`TransportError::Closed`].
    pub async fn send_response<T: Transport + ?Sized>(
        &self,
        transport: Arc<T>,
        message_number: u32,
        payload: Vec<u8>,
    ) {
        let response = Response {
            message_identifier: build_message_identifier(
                RpcMessageTypes::Response as u32,
                message_number,
            ),
            payload,
        };

        if let Err(err) = transport.send(response.encode_to_vec()).await {
            if !matches!(err, TransportError::Closed) {
                error!(
                    "> ServerMessagesHandler > send_response > Error while sending the original response through transport but it seems not to be closed"
                );
                send_remote_error(
                    transport,
                    message_number,
                    ServerError::UnexpectedErrorOnTransport.into(),
                )
                .await;
            } else {
                error!(
                    "> ServerMessagesHandler > send_response > Error while sending response through transport, it seems to be clsoed"
                );
            }
        }
    }

    async fn open_server_stream<T: Transport + ?Sized>(
        &self,
        transport: Arc<T>,
        message_number: u32,
        port_id: u32,
    ) -> ServerResult<OneShotReceiver<Vec<u8>>> {
        let opening_message = StreamMessage {
            closed: false,
            ack: false,
            sequence_id: 0,
            message_identifier: build_message_identifier(
                RpcMessageTypes::StreamMessage as u32,
                message_number,
            ),
            port_id,
            payload: vec![],
        };

        let receiver = self
            .streams_handler
            .send_stream(transport, opening_message)
            .await
            .map_err(|err| {
                if matches!(err, CommonError::TransportWasClosed) {
                    return ServerResultError::Internal(ServerInternalError::TransportWasClosed);
                }
                ServerResultError::Internal(ServerInternalError::TransportError)
            })?;

        Ok(receiver)
    }

    pub async fn register_listener(
        &self,
        message_id: u32,
        callback: UnboundedSender<(RpcMessageTypes, u32, StreamMessage)>,
    ) {
        let mut lock = self.listeners.lock().await;
        lock.insert(message_id, callback);
    }

    pub async fn unregister_listener(&self, message_id: u32) {
        let mut lock = self.listeners.lock().await;
        lock.remove(&message_id);
    }
}

type StreamPackage = (RpcMessageTypes, u32, StreamMessage);

/// Owns the transport a `RpcClient` was built with, and routes each raw response to the
/// listener registered for its message id.
pub struct ClientMessagesHandler<T: Transport + ?Sized> {
    pub transport: Arc<T>,
    pub streams_handler: Arc<StreamsHandler>,
    /// Keyed by request message id; removed once that response arrives. The receiving side
    /// decodes the raw body.
    one_time_listeners: Mutex<HashMap<u32, OneShotSender<Vec<u8>>>>,
    /// Keyed by stream-request message id; fires on every response carrying that id.
    listeners: Mutex<HashMap<u32, UnboundedSender<StreamPackage>>>,
    /// Never triggering it leaves the background task running until the owning `RpcClient` is
    /// dropped.
    process_cancellation_token: CancellationToken,
}

impl<T: Transport + ?Sized + 'static> ClientMessagesHandler<T> {
    pub fn new(transport: Arc<T>) -> Self {
        Self {
            transport,
            one_time_listeners: Mutex::new(HashMap::new()),
            process_cancellation_token: CancellationToken::new(),
            listeners: Mutex::new(HashMap::new()),
            streams_handler: Arc::new(StreamsHandler::new()),
        }
    }

    pub fn start(self: Arc<Self>) {
        let token = self.process_cancellation_token.clone();
        tokio::spawn(async move {
            select! {
                _ = token.cancelled() => {
                    debug!("> ClientMessagesHandler > cancelled!");
                    self.transport.close().await;
                },
                _ = self.process() => {

                }
            }
        });
    }

    pub fn stop(&self) {
        self.process_cancellation_token.cancel();
    }

    async fn process(&self) {
        loop {
            match self.transport.receive().await {
                Ok(data) => {
                    let message_header = parse_header(&data);
                    match message_header {
                        Some((message_type, message_number)) => {
                            let mut read_callbacks = self.one_time_listeners.lock().await;
                            let sender = read_callbacks.remove(&message_number);
                            if let Some(sender) = sender {
                                match sender.send(data) {
                                    Ok(()) => {}
                                    Err(_) => {
                                        error!(
                                            "> ClientMessagesHandler > process > error while sending {} response",
                                            message_number
                                        );
                                        continue;
                                    }
                                }
                            } else {
                                let listeners = self.listeners.lock().await;
                                let listener = listeners.get(&message_number);

                                if let Some(listener) = listener {
                                    if let Ok(stream_message) =
                                        StreamMessage::decode(data.as_slice())
                                    {
                                        if let Err(error) = listener.send((
                                            message_type,
                                            message_number,
                                            stream_message,
                                        )) {
                                            error!(
                                                "> ClientMessagesHandler > process > Error while sending StreamMessage to a listener {error:?}"
                                            )
                                        }
                                    } else {
                                        error!(
                                            "> ClientMessagesHandler > process > Error while decoding bytes into a StreamMessage, something seems to be bad implemented"
                                        )
                                    }
                                } else {
                                    self.streams_handler
                                        .clone()
                                        .message_acknowledged_by_peer(message_number, data)
                                }
                            }
                        }
                        None => {
                            error!(
                                "> ClientMessagesHandler > process > Error on parsing message header - impossible to communicate the error to a listener, the message is corrupted or invalid"
                            );
                            continue;
                        }
                    }
                }
                Err(error) => {
                    error!("> ClientMessagesHandler > process > Error on receive {error:?}");
                    if matches!(error, TransportError::Closed) {
                        info!("> ClientMessagesHandler > process > closing...");
                        break;
                    }
                }
            }
        }
    }

    /// Sending only begins once the server acknowledges the open, and both the wait and the
    /// sending happen in a spawned task.
    pub fn await_server_ack_open_and_send_streams<M: Message + 'static>(
        self: Arc<Self>,
        open_promise: OneShotReceiver<Vec<u8>>,
        client_stream: Generator<M>,
        port_id: u32,
        client_message_id: u32,
    ) {
        let transport = self.transport.clone();
        tokio::spawn(async move {
            match open_promise.await {
                Ok(encoded_ack_response) => {
                    if let Ok(stream_message) =
                        StreamMessage::decode(encoded_ack_response.as_slice())
                    {
                        if stream_message.closed {
                            return;
                        }

                        let new_generator = Generator::from_generator(client_stream, |item| {
                            Some(item.encode_to_vec())
                        });

                        if let Err(error) = self
                            .streams_handler
                            .send_streams_through_transport(
                                transport,
                                new_generator,
                                port_id,
                                client_message_id,
                            )
                            .await
                        {
                            error!(
                                "> ClientMessagesHandler > await_server_ack_open_and_send_streams > Error while executing StreamsHandler::send_streams_through_transport - Error: {error:?}"
                            );
                        }
                    } else {
                        error!(
                            "> ClientMessagesHandler > await_server_ack_open_and_send_streams > Error while decoding bytes into StreamMessage"
                        )
                    }
                }
                Err(_) => {
                    error!(
                        "> ClientMessagesHandler > await_server_ack_open_and_send_streams > Error while awaiting the server to send the ACK for Open stream message, sender half seems to be dropped"
                    );
                }
            }
        });
    }

    pub async fn register_one_time_listener(
        &self,
        message_number: u32,
        callback: OneShotSender<Vec<u8>>,
    ) {
        let mut lock = self.one_time_listeners.lock().await;
        lock.insert(message_number, callback);
    }

    pub async fn unregister_one_time_listener(&self, message_number: u32) {
        self.one_time_listeners.lock().await.remove(&message_number);
    }

    pub async fn register_listener(
        &self,
        message_number: u32,
        callback: UnboundedSender<(RpcMessageTypes, u32, StreamMessage)>,
    ) {
        let mut lock = self.listeners.lock().await;
        lock.insert(message_number, callback);
    }

    pub async fn unregister_listener(&self, message_number: u32) {
        let mut lock = self.listeners.lock().await;
        lock.remove(&message_number);
    }
}

/// Identifies one stream message awaiting its acknowledge: port, message
/// number and sequence id.
type AckKey = (u32, u32, u32);

fn ack_key(port: u32, message: u32, sequence: u32) -> AckKey {
    (port, message, sequence)
}

/// Holds the per-message acknowledge listeners that pace stream sending.
#[derive(Default)]
pub struct StreamsHandler {
    ack_listeners: std::sync::Mutex<HashMap<AckKey, OneShotSender<Vec<u8>>>>,
}

// Synchronous short map lock makes cleanup cancellation-safe on every exit branch.
struct AckReservation<'a> {
    handler: &'a StreamsHandler,
    key: AckKey,
}
impl Drop for AckReservation<'_> {
    fn drop(&mut self) {
        self.handler.ack_listeners.lock().unwrap().remove(&self.key);
    }
}

struct BoundedStreamLifetime<T: Transport + ?Sized + 'static> {
    transport: Arc<T>,
    sequence: Arc<std::sync::atomic::AtomicU32>,
    port: u32,
    message: u32,
    armed: bool,
}
impl<T: Transport + ?Sized + 'static> Drop for BoundedStreamLifetime<T> {
    fn drop(&mut self) {
        if self.armed {
            let transport = self.transport.clone();
            let sequence = self.sequence.load(std::sync::atomic::Ordering::Relaxed);
            let port = self.port;
            let message = self.message;
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    StreamsHandler::close_bounded_stream(transport, sequence, message, port).await;
                });
            }
        }
    }
}

impl StreamsHandler {
    #[cfg(test)]
    pub(crate) fn pending_ack_count(&self) -> usize {
        self.ack_listeners.lock().unwrap().len()
    }
    #[cfg(test)]
    pub(crate) fn complete_test_ack(&self, port: u32, message: u32, sequence: u32, bytes: Vec<u8>) {
        if let Some(sender) = self
            .ack_listeners
            .lock()
            .unwrap()
            .remove(&ack_key(port, message, sequence))
        {
            let _ = sender.send(bytes);
        }
    }
    pub fn new() -> Self {
        Self {
            ack_listeners: std::sync::Mutex::new(HashMap::new()),
        }
    }

    async fn close_stream<T: Transport + ?Sized>(
        &self,
        transport: Arc<T>,
        sequence_id: u32,
        message_number: u32,
        port_id: u32,
    ) -> Result<(), CommonError> {
        let close_message = StreamMessage {
            closed: true,
            ack: false,
            sequence_id,
            message_identifier: build_message_identifier(
                RpcMessageTypes::StreamMessage as u32,
                message_number,
            ),
            port_id,
            payload: vec![],
        };

        transport
            .send(close_message.encode_to_vec())
            .await
            .map_err(|_| CommonError::TransportError)?;

        Ok(())
    }

    /// One message in flight at a time: each [`StreamMessage`] carries the next sequence id
    /// and the next is not sent until the peer acknowledges it. The peer closing the stream
    /// stops the generator; otherwise the stream is closed when the generator runs dry.
    pub async fn send_streams_through_transport<T: Transport + ?Sized + 'static>(
        &self,
        transport: Arc<T>,
        mut stream_generator: Generator<Vec<u8>>,
        port_id: u32,
        message_number: u32,
    ) -> Result<(), CommonError> {
        if stream_generator.delivery_deadline().is_some() {
            return self
                .send_bounded_stream(transport, stream_generator, port_id, message_number)
                .await;
        }
        let mut sequence_number = 0;
        let mut was_closed_by_peer = false;
        while let Some(message) = stream_generator.next().await {
            sequence_number += 1;
            let current_message = StreamMessage {
                closed: false,
                ack: false,
                sequence_id: sequence_number,
                message_identifier: build_message_identifier(
                    RpcMessageTypes::StreamMessage as u32,
                    message_number,
                ),
                port_id,
                payload: message,
            };
            let transport_cloned = transport.clone();

            match self.send_stream(transport_cloned, current_message).await {
                Ok(listener) => {
                    let ack_message = match listener.await {
                        Ok(msg) => match StreamMessage::decode(msg.as_slice()) {
                            Ok(msg) => msg,
                            Err(_) => {
                                error!(
                                    "> StreamsHandler > send_streams_through_transport > Error while decoding bytes into a StreamMessage"
                                );
                                return Err(CommonError::ProtocolError);
                            }
                        },
                        Err(e) => {
                            error!(
                                "> StreamsHandler > send_streams_through_transport > Error while waiting for an ACK Message, the sender half seems to be dropped: {e:?}"
                            );
                            return Err(CommonError::UnexpectedError(
                                "The sender half of a listener seems to be droppped".to_string(),
                            ));
                        }
                    };
                    if ack_message.ack {
                        debug!(
                            "> StreamsHandler > send_streams_through_transport > Listener received the ack for a message, continuing with the next stream"
                        );
                        continue;
                    } else if ack_message.closed {
                        debug!(
                            "> StreamsHandler > send_streams_through_transport > stream was closed by the other peer"
                        );
                        was_closed_by_peer = true;
                        stream_generator.close();
                        break;
                    }
                }
                Err(err) => {
                    error!(
                        "> StreamsHandler > send_streams_through_transport > Error while streaming a server stream {err:?}"
                    );
                    return Err(err);
                }
            }
        }

        if !was_closed_by_peer {
            self.close_stream(transport, sequence_number, message_number, port_id)
                .await?;
        }

        Ok(())
    }

    async fn send_bounded_stream<T: Transport + ?Sized + 'static>(
        &self,
        transport: Arc<T>,
        mut generator: Generator<Vec<u8>>,
        port_id: u32,
        message_number: u32,
    ) -> Result<(), CommonError> {
        use super::stream_protocol::DeliveryCompletion;
        use super::transport::GuardedSend;
        enum Failure {
            Resync,
            Fatal(CommonError),
        }
        let sequence_view = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let mut lifetime = BoundedStreamLifetime {
            transport: transport.clone(),
            sequence: sequence_view.clone(),
            port: port_id,
            message: message_number,
            armed: true,
        };
        let mut sequence = 0;
        while let Some(item) = generator.next_item().await {
            let Some(delivery) = item.delivery else {
                continue;
            };
            let mut completion = item.completion;
            let expires_at = delivery.expires_at;
            let candidate = sequence + 1;
            let message = StreamMessage {
                closed: false,
                ack: false,
                sequence_id: candidate,
                message_identifier: build_message_identifier(
                    RpcMessageTypes::StreamMessage as u32,
                    message_number,
                ),
                port_id,
                payload: item.value,
            };
            let key = ack_key(port_id, message_number, candidate);
            let (tx, rx) = oneshot_channel();
            self.ack_listeners.lock().unwrap().insert(key, tx);
            let _reservation = AckReservation { handler: self, key };
            let handed_off = std::sync::atomic::AtomicBool::new(false);
            let exchange = async {
                match transport
                    .send_guarded(message.encode_to_vec(), delivery)
                    .await
                {
                    Ok(GuardedSend::Suppressed) => return Ok(None),
                    Ok(GuardedSend::Unsupported) => {
                        return Err(Failure::Fatal(CommonError::ProtocolError));
                    }
                    Ok(GuardedSend::Sent) => {
                        handed_off.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    Err(_) => return Err(Failure::Resync),
                }
                let bytes = rx.await.map_err(|_| Failure::Resync)?;
                StreamMessage::decode(bytes.as_slice())
                    .map(Some)
                    .map_err(|_| Failure::Fatal(CommonError::ProtocolError))
            };
            let outcome = tokio::time::timeout_at(expires_at, exchange).await;
            if handed_off.load(std::sync::atomic::Ordering::Relaxed) {
                sequence = candidate;
                sequence_view.store(sequence, std::sync::atomic::Ordering::Relaxed);
            }
            match outcome {
                Ok(Ok(None)) => {
                    if let Some(done) = completion.take() {
                        let _ = done.send(DeliveryCompletion::Suppressed);
                    }
                    continue;
                }
                Ok(Ok(Some(ack))) => {
                    sequence = candidate;
                    if ack.ack {
                        if let Some(done) = completion.take() {
                            let _ = done.send(DeliveryCompletion::Acknowledged);
                        }
                        continue;
                    }
                    if ack.closed {
                        generator.close();
                        lifetime.armed = false;
                        return Ok(());
                    }
                    generator.close();
                    lifetime.armed = false;
                    return Err(CommonError::ProtocolError);
                }
                Ok(Err(Failure::Fatal(error))) => {
                    generator.close();
                    lifetime.armed = false;
                    return Err(error);
                }
                Ok(Err(Failure::Resync)) | Err(_) => {
                    generator.close();
                    break;
                }
            }
        }
        // A clean terminal frame is the existing caller's retry signal. Confirm physical
        // writer completion; failed cleanup retires the socket instead of a fatal RemoteError.
        Self::close_bounded_stream(transport, sequence, message_number, port_id).await;
        lifetime.armed = false;
        Ok(())
    }

    async fn close_bounded_stream<T: Transport + ?Sized>(
        transport: Arc<T>,
        sequence: u32,
        message: u32,
        port: u32,
    ) {
        use super::transport::{GuardedDelivery, GuardedSend};
        let expires_at = tokio::time::Instant::now() + std::time::Duration::from_millis(100);
        let terminal = StreamMessage {
            closed: true,
            ack: false,
            sequence_id: sequence,
            message_identifier: build_message_identifier(
                RpcMessageTypes::StreamMessage as u32,
                message,
            ),
            port_id: port,
            payload: vec![],
        };
        let delivery = GuardedDelivery {
            max_frame_bytes: super::transport::MAX_GUARDED_MESSAGE_BYTES,
            guard: Arc::new(|| Box::pin(async { Ok(true) })),
            expires_at,
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        if !matches!(
            tokio::time::timeout_at(
                expires_at,
                transport.send_guarded(terminal.encode_to_vec(), delivery)
            )
            .await,
            Ok(Ok(GuardedSend::Sent))
        ) {
            transport.close().await;
        }
    }

    /// Returns the acknowledge listener registered for the message. A send failure is reported
    /// to the client as a [`RemoteError`], unless [`Transport::send`] says
    /// [`TransportError::Closed`].
    async fn send_stream<T: Transport + ?Sized>(
        &self,
        transport: Arc<T>,
        message: StreamMessage,
    ) -> Result<OneShotReceiver<Vec<u8>>, CommonError> {
        let (_, message_number) = parse_message_identifier(message.message_identifier);
        let (tx, rx) = oneshot_channel();
        {
            let mut lock = self.ack_listeners.lock().unwrap();
            if lock
                .insert(
                    ack_key(message.port_id, message_number, message.sequence_id),
                    tx,
                )
                .is_some()
            {
                error!(
                    "> StreamsHandler > send_stram > Overriding TX for message_number: {message_number} and sequence_id: {}",
                    message.sequence_id
                );
            }
        }

        if let Err(error) = transport.send(message.encode_to_vec()).await {
            error!(
                "> StreamsHandler > send_stream > Error while sending through transport - message: {message:?} - Error: {error:?}"
            );
            {
                let mut lock = self.ack_listeners.lock().unwrap();
                lock.remove(&ack_key(
                    message.port_id,
                    message_number,
                    message.sequence_id,
                ));
            }
            if !matches!(error, TransportError::Closed) {
                return Err(CommonError::TransportError);
            } else {
                return Err(CommonError::TransportWasClosed);
            }
        }

        Ok(rx)
    }

    /// Releases the sender in [`send_stream`](#method.send_stream) to send the next message.
    pub fn message_acknowledged_by_peer(self: Arc<Self>, message_number: u32, payload: Vec<u8>) {
        tokio::spawn(async move {
            match parse_protocol_message::<StreamMessage>(&payload) {
                Ok((_, _, stream_message)) => {
                    let listener = {
                        let mut lock = self.ack_listeners.lock().unwrap();
                        lock.remove(&ack_key(
                            stream_message.port_id,
                            message_number,
                            stream_message.sequence_id,
                        ))
                    };
                    match listener {
                        Some(sender) => {
                            if sender.send(payload).is_err() {
                                error!(
                                    "> Streams Handler > message_acknowledged_by_peer > Error while sending through the ack listener, seems to be dropped"
                                )
                            }
                        }
                        None => {
                            debug!(
                                "> Streams Handler > message_acknowledged_by_peer > ack listener not found"
                            )
                        }
                    }
                }
                Err(ParseErrors::IsARemoteError((_, remote_error))) => {
                    error!(
                        "> Streams Handler > message_acknowledged_by_peer > Remote Error: {remote_error:?}"
                    )
                }
                Err(err) => {
                    error!(
                        "> Streams Handler > message_acknowledged_by_peer > Error on parsing: {err:?}"
                    );
                }
            }
        });
    }
}

async fn send_remote_error<T: Transport + ?Sized>(
    transport: Arc<T>,
    message_number: u32,
    mut remote_error: RemoteError,
) {
    fill_remote_error(&mut remote_error, message_number);
    if let Err(err) = transport.send(remote_error.encode_to_vec()).await {
        error!(
            "> send_remote_error > Error while sending the remote error through a transport > RemoteError: {remote_error:?} - Error: {err:?}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ack_listeners_are_keyed_by_port_message_and_sequence() {
        let handler = StreamsHandler::new();
        let (first, mut first_rx) = oneshot_channel();
        let (second, mut second_rx) = oneshot_channel();
        {
            let mut listeners = handler.ack_listeners.lock().unwrap();
            listeners.insert(ack_key(1, 7, 1), first);
            listeners.insert(ack_key(2, 7, 1), second);
        }
        handler.complete_test_ack(2, 7, 1, vec![2]);
        assert_eq!(second_rx.try_recv().unwrap(), vec![2]);
        assert!(first_rx.try_recv().is_err());
        assert_eq!(handler.pending_ack_count(), 1);
    }
}
