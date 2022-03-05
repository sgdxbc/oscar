use std::{
    collections::HashMap,
    ops::{Deref, DerefMut},
    sync::Arc,
    time::Duration,
};

use futures::future::BoxFuture;
use rand::{thread_rng, Rng};
use tokio::{
    select, spawn,
    sync::{
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
        Mutex, MutexGuard,
    },
    time::{sleep, sleep_until, Instant},
};

use crate::transport::{self, Config, Receiver};

type Address = String;
type Message = Vec<u8>;

pub struct Transport {
    rx: UnboundedReceiver<(Address, Address, Message, bool)>,
    tx: UnboundedSender<(Address, Address, Message, bool)>,
    recv_table: RecvTable,
    config: Arc<Config<Self>>,
    filter_table: FilterTable,
}
type RecvTable = HashMap<Address, Box<dyn Fn(Address, RxBuffer) + Send>>;
type FilterTable =
    HashMap<u32, Box<dyn Fn(&Address, &Address, &[u8], &mut Duration) -> bool + Send>>;

#[derive(Debug, Clone)]
pub struct RxBuffer(Message);
impl AsRef<[u8]> for RxBuffer {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone)]
pub struct TxAgent {
    tx: UnboundedSender<(Address, Address, Message, bool)>,
    config: Arc<Config<Transport>>,
}

impl transport::TxAgent for TxAgent {
    type Transport = Transport;

    fn config(&self) -> &Config<Self::Transport> {
        &self.config
    }

    fn send_message(
        &self,
        source: &impl Receiver<Self::Transport>,
        dest: &<Self::Transport as transport::Transport>::Address,
        message: impl FnOnce(&mut [u8]) -> u16,
    ) {
        let mut buffer = [0; 9000];
        let message_length = message(&mut buffer);
        let message = buffer[..message_length as usize].to_vec();
        self.tx
            .send((source.get_address().clone(), dest.clone(), message, false))
            .unwrap();
    }
    fn send_message_to_all(
        &self,
        source: &impl Receiver<Self::Transport>,
        message: impl FnOnce(&mut [u8]) -> u16,
    ) {
        let mut buffer = [0; 9000];
        let message_length = message(&mut buffer);
        let message = buffer[..message_length as usize].to_vec();
        for dest in &self.config.replica_address {
            if dest != source.get_address() {
                self.tx
                    .send((
                        source.get_address().clone(),
                        dest.clone(),
                        message.clone(),
                        false,
                    ))
                    .unwrap();
            }
        }
    }
}

impl transport::Transport for Transport {
    type Address = Address;
    type RxBuffer = RxBuffer;
    type TxAgent = TxAgent;

    fn tx_agent(&self) -> Self::TxAgent {
        TxAgent {
            tx: self.tx.clone(),
            config: self.config.clone(),
        }
    }

    fn register(
        &mut self,
        receiver: &impl Receiver<Self>,
        rx_agent: impl Fn(Self::Address, Self::RxBuffer) + 'static + Send,
    ) where
        Self: Sized,
    {
        self.recv_table
            .insert(receiver.get_address().clone(), Box::new(rx_agent));
    }

    fn register_multicast(
        &mut self,
        rx_agent: impl Fn(Self::Address, Self::RxBuffer) + 'static + Send,
    ) {
        todo!()
    }

    fn ephemeral_address(&self) -> Self::Address {
        let mut label = 'A' as u32;
        loop {
            let address = format!("client-{}", char::from_u32(label).unwrap());
            if !self.recv_table.contains_key(&address) {
                return address;
            }
            label += 1;
        }
    }
}

impl Transport {
    pub fn new(n_replica: usize, n_fault: usize) -> Self {
        let config = Config {
            replica_address: (0..n_replica).map(|i| format!("replica-{}", i)).collect(),
            multicast_address: None, // TODO
            n_fault,
        };
        let (tx, rx) = unbounded_channel();
        Self {
            rx,
            tx,
            recv_table: HashMap::new(),
            config: Arc::new(config),
            filter_table: HashMap::new(),
        }
    }

    pub fn client_timeout() -> BoxFuture<'static, ()> {
        // configurable?
        Box::pin(sleep(Duration::from_millis(1000)))
    }

    pub async fn deliver(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        loop {
            select! {
                _ = sleep_until(deadline) => break,
                Some((source, dest, message, filtered)) = self.rx.recv() => {
                    self.deliver_internal(source, dest, message, filtered);
               }
            }
        }
    }

    fn deliver_internal(&self, source: Address, dest: Address, message: Message, filtered: bool) {
        if filtered {
            (self.recv_table.get(&dest).unwrap())(source, RxBuffer(message));
            return;
        }

        let mut delay = Duration::ZERO;
        let mut drop = false;
        for filter in self.filter_table.values() {
            if !filter(&source, &dest, &message, &mut delay) {
                drop = true;
                break;
            }
        }
        println!(
            "{} -> {} [message size = {}] {}",
            source,
            dest,
            message.len(),
            if drop {
                "[drop]".to_string()
            } else {
                format!("[delay = {:?}]", delay)
            }
        );

        if drop {
        } else if delay.is_zero() {
            (self.recv_table.get(&dest).unwrap())(source, RxBuffer(message));
        } else {
            let tx = self.tx.clone();
            spawn(async move {
                sleep(delay).await;
                tx.send((source, dest, message, true)).unwrap();
            });
        }
    }

    pub async fn deliver_now(&mut self) {
        self.deliver(Duration::from_micros(1)).await;
    }

    pub fn insert_filter(
        &mut self,
        filter_id: u32,
        filter: impl Fn(&Address, &Address, &[u8], &mut Duration) -> bool + 'static + Send,
    ) {
        self.filter_table.insert(filter_id, Box::new(filter));
    }

    pub fn remove_filter(&mut self, filter_id: u32) {
        self.filter_table.remove(&filter_id);
    }

    pub fn delay(
        min: Duration,
        max: Duration,
    ) -> impl Fn(&Address, &Address, &[u8], &mut Duration) -> bool + 'static + Send {
        move |_, _, _, delay| {
            *delay += thread_rng().gen_range(min..max); // TODO
            true
        }
    }
}

pub struct Executor<State>(Submit<State>);

impl<S> Executor<S> {
    pub fn new(transport: TxAgent, address: Address, state: S) -> Self {
        Self(Submit {
            state: Arc::new(Mutex::new(state)),
            transport,
            address,
        })
    }

    pub fn with_state(&self, f: impl FnOnce(&StatefulContext<'_, S>)) {
        f(&StatefulContext {
            state: self.0.state.try_lock().unwrap(),
            transport: self.0.transport.clone(),
            submit: self.0.clone(),
        });
    }
}

pub struct StatefulContext<'a, State> {
    state: MutexGuard<'a, State>,
    pub transport: TxAgent,
    pub submit: Submit<State>,
}

impl<'a, S> Receiver<Transport> for StatefulContext<'a, S> {
    fn get_address(&self) -> &Address {
        &self.submit.address
    }
}

impl<'a, S> Deref for StatefulContext<'a, S> {
    type Target = S;
    fn deref(&self) -> &Self::Target {
        &*self.state
    }
}

impl<'a, S> DerefMut for StatefulContext<'a, S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.state
    }
}

pub struct Submit<State> {
    state: Arc<Mutex<State>>,
    transport: TxAgent,
    address: Address,
}

impl<S> Clone for Submit<S> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            transport: self.transport.clone(),
            address: self.address.clone(),
        }
    }
}

impl<S> Submit<S> {
    pub fn stateful(&self, task: impl for<'a> FnOnce(&mut StatefulContext<'a, S>) + Send + 'static)
    where
        S: Send + 'static,
    {
        let submit = self.clone();
        spawn(async move {
            task(&mut StatefulContext {
                state: submit.state.lock().await,
                transport: submit.transport.clone(),
                submit: submit.clone(),
            });
        });
    }
}
