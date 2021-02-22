#pragma once

#include "file_blob.h"
#include "path_range.h"
#include "user_id.h"
#include "versioned_object.h"

#include <set>
#include <map>
#include <string>

namespace ouisync {

class ObjectStore;

class MultiDir {
public:
    struct Version {
        UserId user;
        VersionedObject vobj;
    };

    using Versions = std::map<UserId, VersionedObject>;

public:
    MultiDir(Versions versions, ObjectStore& objstore) :
        versions(std::move(versions)),
        objstore(&objstore)
    {}

    bool has_subdirectory(string_view) const;

    MultiDir cd_into(const std::string& where) const;

    MultiDir cd_into(PathRange path) const;

    std::set<std::string> list() const;

    ObjectId file(const std::string& name) const;

    Opt<Version> pick_subdirectory_to_edit(const UserId& preferred_user, const string_view name);

private:
    std::map<std::string, ObjectId> list_() const;

private:
    Versions versions;
    ObjectStore* objstore;
};

} // namespace
