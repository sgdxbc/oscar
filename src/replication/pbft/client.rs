use std::{collections::HashMap, marker::PhantomData, time::Duration};

use async_trait::async_trait;
use futures::{
    channel::mpsc::{unbounded, UnboundedReceiver},
    StreamExt,
};
use tracing::{debug, warn};

use crate::{
    common::{
        deserialize, generate_id, serialize, ClientId, Opaque, RequestNumber, SignedMessage,
        ViewNumber,
    },
    replication::pbft::message::{self, ToReplica},
    transport::{Receiver, Transport, TxAgent},
    AsyncExecutor, Invoke,
};

pub struct Client<T: Transport, E> {
    address: T::Address,
    pub(super) id: ClientId,
    transport: T::TxAgent,
    rx: UnboundedReceiver<(T::Address, T::RxBuffer)>,
    _executor: PhantomData<E>,

    request_number: RequestNumber,
    view_number: ViewNumber,
}

impl<T: Transport, E> Receiver<T> for Client<T, E> {
    fn get_address(&self) -> &T::Address {
        &self.address
    }
}

impl<T: Transport, E> Client<T, E> {
    pub fn register_new(transport: &mut T) -> Self {
        let (tx, rx) = unbounded();
        let client = Self {
            address: transport.ephemeral_address(),
            id: generate_id(),
            transport: transport.tx_agent(),
            rx,
            request_number: 0,
            view_number: 0,
            _executor: PhantomData,
        };
        transport.register(&client, move |remote, buffer| {
            if tx.unbounded_send((remote, buffer)).is_err() {
                debug!("client channel broken");
            }
        });
        client
    }
}

#[async_trait]
impl<T: Transport, E: for<'a> AsyncExecutor<'a, Opaque>> Invoke for Client<T, E>
where
    Self: Send + Sync,
    E: Send + Sync,
{
    async fn invoke(&mut self, op: Opaque) -> Opaque {
        self.request_number += 1;
        let request = message::Request {
            op,
            request_number: self.request_number,
            client_id: self.id,
        };
        let replica = self.transport.config().view_primary(self.view_number);
        self.transport.send_message_to_replica(
            self,
            replica,
            serialize(ToReplica::Request(request.clone())),
        );

        let mut result_table = HashMap::new();
        let mut receive_buffer =
            move |client: &mut Self, _remote: T::Address, buffer: T::RxBuffer| {
                let reply: SignedMessage<message::Reply> = deserialize(buffer.as_ref()).unwrap();
                let reply = reply.assume_verified();
                if reply.request_number != client.request_number {
                    return None;
                }

                result_table.insert(reply.replica_id, reply.result.clone());
                if reply.view_number > client.view_number {
                    client.view_number = reply.view_number;
                }

                if result_table
                    .values()
                    .filter(|result| **result == reply.result)
                    .count()
                    == client.transport.config().n_fault + 1
                {
                    Some(reply.result)
                } else {
                    None
                }
            };

        loop {
            let receive_loop = async {
                loop {
                    let (remote, buffer) = self.rx.next().await.unwrap();
                    if let Some(result) = receive_buffer(self, remote, buffer) {
                        break result;
                    }
                }
            };
            if let Ok(result) = E::timeout(Duration::from_millis(1000), receive_loop).await {
                return result;
            }

            warn!("resend for request number {}", self.request_number);
            self.transport
                .send_message_to_all(self, serialize(ToReplica::Request(request.clone())));
        }
    }
}