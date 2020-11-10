#include "file_system.h"

#include <iostream>
#include <boost/filesystem.hpp>
#include <boost/range/iterator_range.hpp>

using namespace ouisync;
using std::vector;
using std::map;
using std::string;
using boost::get;

FileSystem::FileSystem(executor_type ex, FileSystemOptions options) :
    _ex(std::move(ex)),
    _options(std::move(options))
{
    _debug_tree = Dir{
        { string("dir"), Dir{ { string("file"), string("content") } } },
        { string("hello"), string("world") }
    };

    _user_id = UserId::load_or_create(_options.user_id_file_path());

    auto branch = Branch::load_or_create(
            _options.branchdir(),
            _options.objectdir(),
            _user_id);

    _branches.insert(std::make_pair(_user_id, std::move(branch)));
}

FileSystem::Tree& FileSystem::find_tree(PathRange path)
{
    auto* tree = &_debug_tree;

    for (auto& p : path) {
        auto dir = get<Dir>(tree);
        assert(dir);
        if (!dir) throw_error(sys::errc::no_such_file_or_directory);
        auto i = dir->find(p.native());
        if (i == dir->end()) throw_error(sys::errc::no_such_file_or_directory);
        tree = &i->second;
    }

    return *tree;
}

template<class T>
T& FileSystem::find(PathRange path_range)
{
    auto p = get<T>(&find_tree(path_range));
    if (!p) throw_error(sys::errc::invalid_argument);
    return *p;
}

Branch& FileSystem::find_branch(PathRange path)
{
    if (path.empty()) throw_error(sys::errc::invalid_argument);
    auto user_id = UserId::from_string(path.front().native());
    if (!user_id) throw_error(sys::errc::invalid_argument);
    auto i = _branches.find(*user_id);
    if (i == _branches.end()) throw_error(sys::errc::invalid_argument);
    return i->second;
}

net::awaitable<FileSystem::Attrib> FileSystem::get_attr(PathRange path)
{
    if (path.empty()) co_return DirAttrib{};

    auto& branch = find_branch(path);

    path.advance_begin(1);
    auto ret = branch.get_attr(path);
    co_return ret;
}

net::awaitable<vector<string>> FileSystem::readdir(PathRange path)
{
    std::vector<std::string> nodes;

    if (path.empty()) {
        for (auto& [name, branch] : _branches) {
            (void) branch;
            nodes.push_back(name.to_string());
        }
    }
    else {
        auto& branch = find_branch(path);

        path.advance_begin(1);
        auto dir = branch.readdir(path);

        for (auto& [name, hash] : dir) {
            nodes.push_back(name);
        }
    }

    co_return nodes;
}

net::awaitable<size_t> FileSystem::read(PathRange path, char* buf, size_t size, off_t offset)
{
    if (path.empty()) {
        throw_error(sys::errc::invalid_argument);
    }

    auto& branch = find_branch(path);
    path.advance_begin(1);

    if (path.empty()) {
        throw_error(sys::errc::is_a_directory);
    }

    co_return branch.read(path, buf, size, offset);
}

net::awaitable<size_t> FileSystem::write(PathRange path, const char* buf, size_t size, off_t offset)
{
    if (path.empty()) {
        throw_error(sys::errc::invalid_argument);
    }

    auto& branch = find_branch(path);
    path.advance_begin(1);

    if (path.empty()) {
        throw_error(sys::errc::is_a_directory);
    }

    co_return branch.write(path, buf, size, offset);
}

net::awaitable<void> FileSystem::mknod(PathRange path, mode_t mode, dev_t dev)
{
    if (S_ISFIFO(mode)) throw_error(sys::errc::invalid_argument); // TODO?

    if (path.empty()) {
        throw_error(sys::errc::invalid_argument);
    }

    auto& branch = find_branch(path);
    path.advance_begin(1);

    if (path.empty()) {
        throw_error(sys::errc::is_a_directory);
    }

    branch.store(path, object::Blob{});

    co_return;
}

net::awaitable<void> FileSystem::mkdir(PathRange path, mode_t mode)
{
    if (path.empty()) {
        // The root directory is reserved for branches, users can't create
        // new directories there.
        throw_error(sys::errc::operation_not_permitted);
    }

    auto& branch = find_branch(path);
    path.advance_begin(1);
    branch.mkdir(path);
    co_return;
}

net::awaitable<void> FileSystem::remove_file(PathRange path)
{
    if (path.empty()) {
        throw_error(sys::errc::is_a_directory);
    }

    auto& branch = find_branch(path);
    path.advance_begin(1);

    if (path.empty()) {
        // XXX: Branch removal not yet implemented
        throw_error(sys::errc::operation_not_permitted);
    }

    branch.remove(path);
    co_return;
}

net::awaitable<void> FileSystem::remove_directory(PathRange path)
{
    if (path.empty()) {
        // XXX: Branch removal not yet implemented
        throw_error(sys::errc::operation_not_permitted);
    }

    auto& branch = find_branch(path);
    path.advance_begin(1);

    if (path.empty()) {
        // XXX: Branch removal not yet implemented
        throw_error(sys::errc::operation_not_permitted);
    }

    branch.rmdir(path);
    co_return;
}

net::awaitable<size_t> FileSystem::truncate(PathRange path, size_t size)
{
    if (path.empty()) {
        // XXX: Branch removal not yet implemented
        throw_error(sys::errc::is_a_directory);
    }

    auto& branch = find_branch(path);
    path.advance_begin(1);

    if (path.empty()) {
        // XXX: Branch removal not yet implemented
        throw_error(sys::errc::is_a_directory);
    }

    co_return branch.truncate(path, size);
}
