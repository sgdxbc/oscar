#pragma once
#include <limits>
#include <vector>

#include <rte_byteorder.h>
#include <rte_eal.h>
#include <rte_ethdev.h>
#include <rte_ether.h>
#include <rte_launch.h>
#include <rte_mbuf.h>
#include <rte_timer.h>

#include "core/Foundation.hpp"

// Adapt from dpdk/examples/skeleton/basicfwd.c

#define RX_RING_SIZE 1024
#define TX_RING_SIZE 1024

#define NUM_MBUFS 8191
#define MBUF_CACHE_SIZE 250
#define BURST_SIZE 32

/* basicfwd.c: Basic DPDK skeleton forwarding example. */

/*
 * Initializes a given port using global settings and with the RX buffers
 * coming from the mbuf_pool passed as a parameter.
 */

/* Main functional part of port initialization. 8< */
static inline int port_init(
    uint16_t port, struct rte_mempool *mbuf_pool, struct rte_ether_addr &addr)
{
    struct rte_eth_conf port_conf;
    const uint16_t rx_rings = rte_lcore_count(), tx_rings = rte_lcore_count();
    uint16_t nb_rxd = RX_RING_SIZE;
    uint16_t nb_txd = TX_RING_SIZE;
    int retval;
    uint16_t q;
    struct rte_eth_dev_info dev_info;
    struct rte_eth_txconf txconf;

    if (!rte_eth_dev_is_valid_port(port))
        return -1;

    memset(&port_conf, 0, sizeof(struct rte_eth_conf));

    retval = rte_eth_dev_info_get(port, &dev_info);
    if (retval != 0) {
        printf(
            "Error during getting device (port %u) info: %s\n", port,
            strerror(-retval));
        return retval;
    }

    if (dev_info.tx_offload_capa & RTE_ETH_TX_OFFLOAD_MBUF_FAST_FREE)
        port_conf.txmode.offloads |= RTE_ETH_TX_OFFLOAD_MBUF_FAST_FREE;

    /* Configure the Ethernet device. */
    retval = rte_eth_dev_configure(port, rx_rings, tx_rings, &port_conf);
    if (retval != 0)
        return retval;

    retval = rte_eth_dev_adjust_nb_rx_tx_desc(port, &nb_rxd, &nb_txd);
    if (retval != 0)
        return retval;

    /* Allocate and set up 1 RX queue per lcore. */
    for (q = 0; q < rx_rings; q++) {
        retval = rte_eth_rx_queue_setup(
            port, q, nb_rxd, rte_eth_dev_socket_id(port), NULL, mbuf_pool);
        if (retval < 0)
            return retval;
    }

    txconf = dev_info.default_txconf;
    txconf.offloads = port_conf.txmode.offloads;
    /* Allocate and set up 1 TX queue per lcore. */
    for (q = 0; q < tx_rings; q++) {
        retval = rte_eth_tx_queue_setup(
            port, q, nb_txd, rte_eth_dev_socket_id(port), &txconf);
        if (retval < 0)
            return retval;
    }

    /* Starting Ethernet port. 8< */
    retval = rte_eth_dev_start(port);
    /* >8 End of starting of ethernet port. */
    if (retval < 0)
        return retval;

    /* Display the port MAC address. */
    retval = rte_eth_macaddr_get(port, &addr);
    if (retval != 0)
        return retval;

    printf(
        "Port %u MAC: %02" PRIx8 " %02" PRIx8 " %02" PRIx8 " %02" PRIx8
        " %02" PRIx8 " %02" PRIx8 "\n",
        port, RTE_ETHER_ADDR_BYTES(&addr));

    /* Enable RX in promiscuous mode for the Ethernet device. */
    retval = rte_eth_promiscuous_enable(port);
    /* End of setting RX port in promiscuous mode. */
    if (retval != 0)
        return retval;

    return 0;
}
/* >8 End of main functional part of port initialization. */
// ----

namespace oskr
{
class DPDKClient;

struct MBufDesc {
    DPDKClient &transport;
    struct rte_mbuf *mbuf;

    explicit MBufDesc(struct rte_mbuf *mbuf, DPDKClient &transport) :
        transport(transport)
    {
        this->mbuf = mbuf;
    }
    ~MBufDesc();

    MBufDesc(const MBufDesc &) = delete;
    MBufDesc(const MBufDesc &&desc) : transport(desc.transport)
    {
        mbuf = desc.mbuf;
    }

    std::uint8_t *data() const
    {
        return rte_pktmbuf_mtod_offset(mbuf, std::uint8_t *, 18);
    }
    std::size_t size() const { return rte_pktmbuf_data_len(mbuf) - 18; }
};

template <> struct TransportMeta<DPDKClient> {
    using Address = std::pair<struct rte_ether_addr, std::uint16_t>;
    static constexpr std::size_t buffer_size = RTE_MBUF_DEFAULT_DATAROOM - 18;
    using Desc = MBufDesc;
};

bool operator==(
    const TransportMeta<DPDKClient>::Address &lhs,
    const TransportMeta<DPDKClient>::Address &rhs)
{
    return rte_is_same_ether_addr(&lhs.first, &rhs.first) &&
           lhs.second == rhs.second;
}

/*! Transport implementation based on DPDK and designed for clients.

Notable features/assumptions:
- No task-level parallization, i.e., every receiver is (almost) pinned to one
lcore worker.
- Timeout resolution is 1 millisecond.
- Non-locking receiver state accessing (i.e., closure calling). All packets
arriving the same receiver will be rx by the same lcore, and the lcore worker
finish all `spawn`ed tasks before processing the next packet, so there is no
cross-packet state race by nature.
- No multicast receiver support.
- No dynamical rx buffering. Even if receiver closure is lightweight, if spawned
task stucks, rx packet will still be dropped. Actually, spawned task is expected
to be lightweight as well.

The transport implementation promises that receiver closure will always be
called from the same lcore worker, except one case: the external `spawn` call
before calling `run` from main thread, which normally used for "igniting" the
close loop client. That callback of `spawn`, including the `invoke` calling
which is probably inside, will be executed by the main thread.

Because the implementation of `spawnConcurrent` is actually sequential, it is
meaningless in the sense of improving throughput performance. See `BasicClient`
as an example of receiver suitable for used with `DPDKClient`.
*/
class DPDKClient : public TransportBase<DPDKClient>
{
    struct rte_mempool *pool;
    struct rte_ether_addr mac_address;
    std::vector<Receiver> receiver_list;
    volatile bool force_quit;

    struct TimerBox {
        struct rte_timer timer;
        Callback callback;
    };

public:
    const Config<DPDKClient> &config;

    DPDKClient(Config<DPDKClient> &config, char *prog_name);

    ~DPDKClient() { rte_eal_cleanup(); }

    void registerReceiver(Address address, Receiver receiver)
    {
        if (address != allocateAddress()) {
            panic("register receiver with unexpected address");
        }
        receiver_list.push_back(receiver);
    }

    void registerMulticastReceiver(Receiver)
    {
        panic("unsupported: register multicast receiver");
    }

    void spawn(Callback) { panic("Todo"); }

    void spawnConcurrent(Callback callback) { spawn(std::move(callback)); }

    Address allocateAddress() { return {mac_address, receiver_list.size()}; }

    template <typename Sender>
    void sendMessage(const Sender &sender, const Address &dest, Write write)
    {
        struct rte_mbuf *mbuf = buildTXMBuf(sender.address, dest, write);
        if (rte_eth_tx_burst(0, 0, &mbuf, 1) != 1) {
            panic("failed to send message");
        }
    }

    template <typename Sender>
    void sendMessageToAll(const Sender &sender, Write write)
    {
        // this could be unpractical if decide to use i32 as ReplicaId some
        // day...
        struct rte_mbuf *pkts[std::numeric_limits<ReplicaId>::max()];
        int n_pkt = 0;
        for (auto dest : config.replica_address_list) {
            if (dest != sender.address) {
                if (n_pkt == 0) {
                    pkts[n_pkt] = buildTXMBuf(sender.address, dest, write);
                } else {
                    struct rte_mbuf *mbuf =
                        rte_pktmbuf_copy(pkts[0], pool, 0, UINT32_MAX);
                    struct rte_ether_hdr *ether =
                        rte_pktmbuf_mtod(mbuf, struct rte_ether_hdr *);
                    rte_ether_addr_copy(&dest.first, &ether->dst_addr);
                    *rte_pktmbuf_mtod_offset(mbuf, rte_be16_t *, 14) =
                        rte_cpu_to_be_16(dest.second);
                    pkts[n_pkt] = mbuf;
                }
                n_pkt += 1;
            }
        }
        if (rte_eth_tx_burst(0, 0, pkts, n_pkt) != n_pkt) {
            panic("failed to send message to all");
        }
    }

    FnOnce<void()> spawn(std::chrono::microseconds delay, Callback callback)
    {
        TimerBox *box = new TimerBox;
        rte_timer_init(&box->timer);
        box->callback = std::move(callback);
        rte_timer_reset(
            &box->timer, 0, SINGLE, rte_lcore_id(), timerCallback, box);
        return [box] {
            rte_timer_stop(&box->timer);
            delete box;
        };
    }

    // maybe better to use friend?
    void releaseDescriptor(MBufDesc &desc) { rte_pktmbuf_free(desc.mbuf); }

    void run()
    {
        force_quit = false;
        rte_eal_mp_remote_launch(lcoreMainLoop, this, CALL_MAIN);
        rte_eal_mp_wait_lcore();
    }

    void stop() { force_quit = true; }

private:
    struct rte_mbuf *
    buildTXMBuf(const Address &source, const Address &dest, Write write);

    static void timerCallback(struct rte_timer *, void *arg)
    {
        TimerBox *box = (TimerBox *)arg;
        Callback callback = std::move(box->callback);
        rte_timer_stop(&box->timer);
        delete box;
        callback();
    }

    static int lcoreMainLoop(void *arg)
    {
        ((DPDKClient *)arg)->mainLoop();
        return 0;
    }

    void mainLoop();
};

MBufDesc::~MBufDesc() { transport.releaseDescriptor(*this); }

DPDKClient::DPDKClient(Config<DPDKClient> &config, char *prog_name) :
    config(config)
{
    std::vector<char *> args{prog_name};
    if (rte_eal_init(args.size(), args.data()) < 0) {
        panic("EAL initialize failed");
    }
    unsigned n_port = rte_eth_dev_count_avail();
    if (n_port == 0) {
        panic("no available port");
    }
    if (n_port > 1) {
        warn("multiple available port, only 1st port will be used");
    }

    pool = rte_pktmbuf_pool_create(
        "MBUF_POOL", NUM_MBUFS * n_port, MBUF_CACHE_SIZE, 0,
        RTE_MBUF_DEFAULT_BUF_SIZE, rte_socket_id());
    if (pool == NULL) {
        panic("cannot create mbuf pool");
    }

    std::uint16_t port_id = rte_eth_find_next(0);
    if (port_init(port_id, pool, mac_address) != 0) {
        panic("cannot init port {}", port_id);
    }
    // TODO setup flow policy

    // non-dpdk setup
    // insert a placeholder receiver, so client address starts from 1
    receiver_list.push_back([](const auto &, auto) { panic("unreachable"); });
}

struct rte_mbuf *
DPDKClient::buildTXMBuf(const Address &source, const Address &dest, Write write)
{
    struct rte_mbuf *mbuf = rte_pktmbuf_alloc(pool);
    struct rte_ether_hdr *ether =
        rte_pktmbuf_mtod(mbuf, struct rte_ether_hdr *);
    rte_ether_addr_copy(&dest.first, &ether->dst_addr);
    rte_ether_addr_copy(&source.first, &ether->src_addr);
    // https://stackoverflow.com/a/52682687
    ether->ether_type = rte_cpu_to_be_16(0x88d5);

    *rte_pktmbuf_mtod_offset(mbuf, rte_be16_t *, 14) =
        rte_cpu_to_be_16(dest.second);
    *rte_pktmbuf_mtod_offset(mbuf, rte_be16_t *, 16) =
        rte_cpu_to_be_16(source.second);

    mbuf->pkt_len = mbuf->data_len =
        18 + write(TxSpan<buffer_size>(
                 rte_pktmbuf_mtod_offset(mbuf, uint8_t *, 18), buffer_size));
    return mbuf;
}

void DPDKClient::mainLoop()
{
    const std::uint16_t port_id = rte_eth_find_next(0);
    // TODO get queue id
    const std::uint16_t queue_id = 0;
    while (!force_quit) {
        // TODO timer

        struct rte_mbuf *pkts[BURST_SIZE];
        const std::uint16_t n_rx =
            rte_eth_rx_burst(port_id, queue_id, pkts, BURST_SIZE);
        for (std::uint16_t i = 0; i < n_rx; i += 1) {
            struct rte_mbuf *pkt = pkts[i];
            Address remote;
            rte_ether_addr_copy(
                &rte_pktmbuf_mtod(pkt, struct rte_ether_hdr *)->src_addr,
                &remote.first);
            remote.second = rte_be_to_cpu_16(
                *rte_pktmbuf_mtod_offset(pkt, rte_be16_t *, 16));

            if (remote.second >= receiver_list.size()) {
                warn("unregistered: local id = {}", remote.second);
                rte_pktmbuf_free(pkt);
                continue;
            }
            receiver_list.at(remote.second)(remote, MBufDesc(pkt, *this));
            // TODO finish all spawned tasks
        }
    }
}

} // namespace oskr