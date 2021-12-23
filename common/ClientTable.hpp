#pragma once
#include <optional>
#include <unordered_map>

#include "core/Foundation.hpp"

namespace oscar
{
template <typename Transport, typename ReplyMessage> class ClientTable
{
    struct Record {
        std::optional<typename Transport::Address> remote;
        RequestNumber request_number;
        std::optional<ReplyMessage> reply_message;
    };

    std::unordered_map<ClientId, Record> record_table;

public:
    using Apply = std::function<void(
        std::function<void(
            const typename Transport::Address &remote, const ReplyMessage &)>)>;

    //! On direct request from client.
    //!
    Apply check(
        const typename Transport::Address &remote, ClientId client_id,
        RequestNumber request_number)
    {
        auto iter = record_table.find(client_id);
        if (iter == record_table.end()) {
            record_table.insert(
                {client_id, {remote, request_number, std::nullopt}});
            return nullptr;
        }

        auto &record = iter->second;
        if (request_number < record.request_number) {
            return [](auto) {};
        }
        if (request_number == record.request_number) {
            if (!record.reply_message) {
                return [](auto) {};
            }
            return [remote, reply = *record.reply_message](auto on_reply) {
                on_reply(remote, reply);
            };
        }

        if (request_number != record.request_number + 1) {
            panic(
                "Not continuous request number: client id = {}, {} -> {}",
                client_id, record.request_number, request_number);
        }

        record.request_number = request_number;
        record.reply_message = std::nullopt;
        return nullptr;
    }

    //! On relayed request message.
    //!
    void update(ClientId client_id, RequestNumber request_number)
    {
        auto iter = record_table.find(client_id);
        if (iter == record_table.end()) {
            record_table.insert(
                {client_id, {std::nullopt, request_number, std::nullopt}});
            return;
        }

        if (iter->second.request_number >= request_number) {
            warn(
                "Ignore late update (request): client id = {:x}, request "
                "number = {}, recorded request = {}",
                client_id, request_number, iter->second.request_number);
            return;
        }

        iter->second.request_number = request_number;
        iter->second.reply_message.reset();
    }

    Apply
    update(ClientId client_id, RequestNumber request_number, ReplyMessage reply)
    {
        auto iter = record_table.find(client_id);
        if (iter == record_table.end()) {
            warn("No record: client id = {:x}", client_id);
            record_table.insert(
                {client_id, {std::nullopt, request_number, reply}});
            return [](auto) {};
        }

        if (iter->second.request_number > request_number) {
            warn(
                "Ignore late update: client id = {:x}, request number = {}, "
                "recorded request = {}",
                client_id, request_number, iter->second.request_number);
            return [](auto) {};
        }
        if (iter->second.request_number < request_number) {
            warn(
                "Outdated local record: client id = {:x}, request number = {}, "
                "recorded request = {}",
                client_id, request_number, iter->second.request_number);
            iter->second.request_number = request_number;
        }

        iter->second.reply_message = reply;
        if (!iter->second.remote) {
            debug("Client address not recorded: id = {:x}", client_id);
            return [](auto) {};
        }

        return [remote = *iter->second.remote, reply](auto on_reply) {
            on_reply(remote, reply);
        };
    }
};
} // namespace oscar