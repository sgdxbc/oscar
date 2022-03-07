use std::{
    cell::RefCell,
    ffi::c_void,
    future::Future,
    mem::take,
    pin::Pin,
    process,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Barrier, Mutex,
    },
    task::{Context, Poll},
    time::Duration,
};

use futures::{channel::oneshot, future::BoxFuture, select, task::noop_waker_ref, FutureExt};
use hdrhistogram::Histogram;
use oskr::{
    common::{panic_abort, Opaque},
    dpdk::Transport,
    dpdk_shim::{rte_eal_mp_remote_launch, rte_rmt_call_main_t},
    replication::unreplicated,
    transport::{Config, Receiver},
    AsyncExecutor as _, Invoke,
};
use quanta::{Clock, Instant};

struct AsyncExecutor;
impl AsyncExecutor {
    thread_local! {
        static TASK_LIST: RefCell<Vec<BoxFuture<'static, ()>>> = RefCell::new(Vec::new());
    }
    fn poll_now() {
        Self::TASK_LIST.with(|task_list| {
            for task in &mut *task_list.borrow_mut() {
                let _ = Pin::new(task).poll(&mut Context::from_waker(noop_waker_ref()));
            }
        });
    }
    fn poll(duration: Duration) {
        let clock = Clock::new();
        let start = clock.start();
        while clock.delta(start, clock.end()) < duration {
            Self::poll_now();
        }
    }
}

struct Timeout<'a, T>(BoxFuture<'a, T>, Instant);
struct Elapsed;
impl<'a, T> Future for Timeout<'a, T> {
    type Output = Result<T, Elapsed>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if Instant::now() > self.1 {
            Poll::Ready(Err(Elapsed))
        } else {
            Pin::new(&mut self.0).poll(cx).map(Result::Ok)
        }
    }
}

impl<'a, T: Send + 'static> oskr::AsyncExecutor<'a, T> for AsyncExecutor {
    type JoinHandle = BoxFuture<'static, T>; // too lazy to invent new type
    type Timeout = BoxFuture<'a, Result<T, Self::Elapsed>>;
    type Elapsed = Elapsed;
    fn spawn(task: impl Future<Output = T> + Send + 'static) -> Self::JoinHandle {
        let (tx, rx) = oneshot::channel();
        Self::TASK_LIST.with(|task_list| {
            task_list.borrow_mut().push(Box::pin(async move {
                tx.send(task.await).map_err(|_| "send fail").unwrap();
            }))
        });
        Box::pin(async move { rx.await.unwrap() })
    }
    fn timeout(duration: Duration, task: impl Future<Output = T> + Send + 'a) -> Self::Timeout {
        Box::pin(Timeout(Box::pin(task), Instant::now() + duration))
    }
}

fn main() {
    tracing_subscriber::fmt::init();
    panic_abort();

    let port_id = 0;
    let n_worker = 20;
    let n_client = 20;
    let duration_second = 10;
    let config = Config {
        replica_address: vec!["b8:ce:f6:2a:2f:94#0".parse().unwrap()],
        n_fault: 0,
        multicast_address: None,
    };

    let mut transport = Transport::setup(config, port_id, 1, n_worker);
    let k = (n_client - 1) / n_worker + 1;
    let client_list: Vec<Vec<_>> = (0..n_worker)
        .map(|i| {
            (i * k..n_client.min((i + 1) * k))
                // TODO select client type
                .map(|_| unreplicated::Client::<_, AsyncExecutor>::register_new(&mut transport))
                .collect()
        })
        .collect();

    struct WorkerData<Client> {
        client_list: Vec<Vec<Client>>,
        count: Arc<AtomicU32>,
        duration_second: u64,
        hist: Arc<Mutex<Histogram<u64>>>,
        barrier: Arc<Barrier>,
    }
    let mut worker_data = WorkerData {
        client_list,
        count: Arc::new(AtomicU32::new(0)),
        duration_second,
        hist: Arc::new(Mutex::new(Histogram::new(2).unwrap())),
        barrier: Arc::new(Barrier::new(n_worker as usize)),
    };
    extern "C" fn worker<Client: Receiver<Transport> + Invoke + Send + 'static>(
        arg: *mut c_void,
    ) -> i32 {
        // TODO take a safer shared reference
        let worker_data: &mut WorkerData<Client> = unsafe { &mut *(arg as *mut _) };
        let worker_id = Transport::worker_id();
        let client_list = if let Some(client_list) = worker_data.client_list.get_mut(worker_id) {
            take(client_list)
        } else {
            return 0;
        };
        let count = worker_data.count.clone();
        let duration_second = worker_data.duration_second;
        let barrier = worker_data.barrier.clone();
        let hist = worker_data.hist.clone();

        // I want a broadcast :|
        let (client_list, shutdown_list): (Vec<_>, Vec<_>) = client_list
            .into_iter()
            .map(|mut client| {
                let count = count.clone();
                let (shutdown_tx, mut shutdown) = oneshot::channel();
                (
                    AsyncExecutor::spawn(async move {
                        println!("{}", client.get_address());
                        let clock = Clock::new();
                        let mut hist: Histogram<u64> = Histogram::new(2).unwrap();
                        loop {
                            let start = clock.start();
                            select! {
                                _ = client.invoke(Opaque::default()).fuse() => {},
                                _ = shutdown => return hist,
                            }
                            let end = clock.end();
                            hist += clock.delta(start, end).as_nanos() as u64;
                            count.fetch_add(1, Ordering::SeqCst);
                        }
                    }),
                    shutdown_tx,
                )
            })
            .unzip();

        if worker_id == 0 {
            for _ in 0..duration_second {
                AsyncExecutor::poll(Duration::from_secs(1));
                let count = count.swap(0, Ordering::SeqCst);
                println!("{}", count);
            }
        } else {
            AsyncExecutor::poll(Duration::from_secs(duration_second));
        }

        for shutdown in shutdown_list {
            shutdown.send(()).unwrap();
        }
        AsyncExecutor::poll_now();

        let mut worker_hist = Histogram::new(2).unwrap();
        for mut client in client_list {
            if let Poll::Ready(hist) =
                Pin::new(&mut client).poll(&mut Context::from_waker(noop_waker_ref()))
            {
                worker_hist += hist;
            } else {
                unreachable!();
            }
        }

        *hist.lock().unwrap() += worker_hist;
        if barrier.wait().is_leader() {
            let hist = hist.lock().unwrap();
            println!(
                "min {:?} p50 {:?} mean {:?} p99 {:?}",
                Duration::from_nanos(hist.min()),
                Duration::from_nanos(hist.value_at_quantile(0.5)),
                Duration::from_nanos(hist.mean() as u64),
                Duration::from_nanos(hist.value_at_quantile(0.99))
            );
            process::exit(0); // TODO more graceful
        } else {
            0
        }
    }
    unsafe {
        rte_eal_mp_remote_launch(
            worker::<unreplicated::Client<Transport, AsyncExecutor>>,
            &mut worker_data as *mut _ as *mut _,
            rte_rmt_call_main_t::SKIP_MAIN,
        );
    }

    transport.run(0);
}
