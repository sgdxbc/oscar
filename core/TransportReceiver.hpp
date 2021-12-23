#pragma once
#include "core/Type.hpp"

namespace oscar
{
template <typename Transport> class TransportReceiver
{
public:
    const typename Transport::Address address;
    TransportReceiver(typename Transport::Address address) : address(address) {}
    virtual ~TransportReceiver() {}

    virtual void
    receiveMessage(const typename Transport::Address &remote, Span span) = 0;

    // the codebase assume at most one multicast address present
    virtual void receiveMulticastMessage(
        const typename Transport::Address &remote, Span span)
    {
        receiveMessage(remote, span);
    }
};

} // namespace oscar