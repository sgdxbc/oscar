#include <gtest/gtest.h>

#include "common/BasicClient.hpp"
#include "transport/Simulated.hpp"

using namespace oskr; // NOLINT
using ReplicaMessage = std::variant<RequestMessage>;

TEST(BasicClient, Noop)
{
    Config<Simulated> config{0, {}, {}};
    Simulated transport(config);
    BasicClient<Simulated, ReplicaMessage> client(
        transport,
        {BasicClient<
             Simulated, ReplicaMessage>::Config::Strategy::PRIMARY_FIRST,
         1000ms, 1});
}
