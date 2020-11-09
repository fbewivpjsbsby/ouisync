#include "fuse_runner.h"
#include "file_system.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <assert.h>
#include <iostream>

#include "defer.h"

#include <boost/asio/signal_set.hpp>
#include <boost/asio/detached.hpp>
#include <boost/asio/co_spawn.hpp>

using namespace ouisync;

FuseRunner::FuseRunner(FileSystem& fs, fs::path mountdir) :
    _fs(fs),
    _mountdir(std::move(mountdir)),
    _work(net::make_work_guard(_fs.get_executor()))
{
    const struct fuse_operations _fuse_oper = {
        .getattr   = _fuse_getattr,
        .mknod     = _fuse_mknod,
        .mkdir     = _fuse_mkdir,
        .unlink    = _fuse_unlink, // remove file
        .rmdir     = _fuse_rmdir,
        .truncate  = _fuse_truncate,
        .utime     = _fuse_utime,
        .open      = _fuse_open,
        .read      = _fuse_read,
        .write     = _fuse_write,
        .readdir   = _fuse_readdir,
        .init      = _fuse_init,
    };

    static const char* argv[] = { "ouisync" };

    _fuse_args = FUSE_ARGS_INIT(1, (char**) argv);
    auto free_args_on_exit = defer([&] { fuse_opt_free_args(&_fuse_args); });

    _fuse_channel = fuse_mount(_mountdir.c_str(), &_fuse_args);

    if (!_fuse_channel) {
        fuse_opt_free_args(&_fuse_args);
        throw std::runtime_error("FUSE: Failed to mount");
    }

    _fuse = fuse_new(_fuse_channel, &_fuse_args, &_fuse_oper, sizeof(_fuse_oper), this);

    if (!_fuse) {
        fuse_unmount(_mountdir.c_str(), _fuse_channel);
        fuse_opt_free_args(&_fuse_args);
        throw std::runtime_error("FUSE: failed in fuse_new");
    }

    _thread = std::thread([this] { run_loop(); });
}

static FuseRunner* _get_self()
{
    return reinterpret_cast<FuseRunner*>(fuse_get_context()->private_data);
}

/* static */
void* FuseRunner::_fuse_init(struct fuse_conn_info *conn)
{
    (void) conn;
    return _get_self();
}

template<class F, class R>
/* static */
Result<R> FuseRunner::query_fs(const char* fname, F&& f) {
    FuseRunner* self = _get_self();
    auto& fs = self->_fs;
    auto ex = fs.get_executor();

    std::mutex m;
    m.lock();
    Result<R> ret = R{};

    co_spawn(ex, [&] () -> net::awaitable<void> {
        try {
            ret = co_await f(fs);
        }
        catch (const sys::system_error& e) {
            ret = outcome::failure(e.code());
        }
        catch (const std::exception& e) {
            std::cerr << "FileSystem has thrown a non system exception "
                "when calling '" << fname << "': "
                << e.what() << ". FuseRunner doens't know how to deal with it. "
                "Exiting\n";
            exit(1);
        }
    }, [&] (auto) {
        m.unlock();
    });

    // Wait for the mutex to get unlocked
    auto lock = std::scoped_lock<std::mutex>(m);

    return ret;
}

#define QUERY_FS(f) query_fs(__PRETTY_FUNCTION__, f)

static fs::path remove_root(const char* path)
{
    assert(*path == '/');
    return fs::path(path + 1);
}

template<class Result>
static void report_if_error(const char* function_name, const Result& r, const char* path)
{
    if (r) return;
    std::cerr << "FUSE: Error in function '" <<
        function_name << "' path '" <<
        path << "' error: " << r.error().message() << "\n";
}

/* static */
int FuseRunner::_fuse_getattr(const char *path_, struct stat *stbuf)
{
    fs::path path = remove_root(path_);

    auto attr = QUERY_FS([&] (auto& fs) {
        return fs.get_attr(path_range(path));
    });

    // System asks for a lot of files that don't exist right after mounting and
    // also each time bash enters a directory. That causes a lot of noise from
    // the below report_if_error function. So it's commented out.
    //report_if_error("getattr", attr, path_);

    if (!attr) return -ENOENT;

    apply(attr.value(),
            [&] (FileSystem::DirAttr) {
                stbuf->st_mode = S_IFDIR | 0755;
                stbuf->st_nlink = 1;
            },
            [&] (FileSystem::FileAttr a) {
                stbuf->st_mode = S_IFREG | 0444;
                stbuf->st_nlink = 1;
                stbuf->st_size = a.size;
            });

    return 0;
}

/* static */
int FuseRunner::_fuse_readdir(const char *path_, void *buf, fuse_fill_dir_t filler,
                         off_t offset, struct fuse_file_info *fi)
{
    (void) offset;
    (void) fi;

    fs::path path = remove_root(path_);

    auto direntries = QUERY_FS([&] (auto& fs) {
        return fs.readdir(path_range(path));
    });

    report_if_error("readdir", direntries, path_);

    if (!direntries) {
        assert(direntries.error().value() == ENOENT);
        return - direntries.error().value();
    }

    filler(buf, ".", NULL, 0);
    filler(buf, "..", NULL, 0);

    for (auto& e : direntries.value()) {
        filler(buf, e.c_str(), NULL, 0);
    }

    return 0;
}

/* static */
int FuseRunner::_fuse_open(const char *path_, struct fuse_file_info *fi)
{
    fs::path path = remove_root(path_);

    auto is_file_result = QUERY_FS([&] (auto& fs) -> net::awaitable<bool> {
        auto attr = co_await fs.get_attr(path_range(path));
        co_return bool(boost::get<FileSystem::FileAttr>(&attr));
    });

    report_if_error("open", is_file_result, path_);

    if (!is_file_result) return - is_file_result.error().value();

    // TODO: Documentations says the app may pass O_TRUNC here, so we should handle it.

    // TODO:
    //if ((fi->flags & O_ACCMODE) != O_RDONLY)
    //    return -EACCES;

    return 0;
}

/* static */
int FuseRunner::_fuse_read(const char *path_, char *buf, size_t size, off_t offset,
                      struct fuse_file_info*)
{
    fs::path path = remove_root(path_);
    auto rs = QUERY_FS([&] (auto& fs) { return fs.read(path_range(path), buf, size, offset); });
    return rs ? rs.value() : -rs.error().value();
}

/* static */
int FuseRunner::_fuse_write(
        const char* path_,
        const char* buf,
        size_t size,
        off_t offset,
        struct fuse_file_info* fi)
{
    fs::path path = remove_root(path_);
    auto rs = QUERY_FS([&] (auto& fs) { return fs.write(path_range(path), buf, size, offset); });
    report_if_error("write", rs, path_);
    return rs ? rs.value() : -rs.error().value();
}

/* static */
int FuseRunner::_fuse_truncate(const char *path_, off_t offset)
{
    fs::path path = remove_root(path_);
    auto rs = QUERY_FS([&] (auto& fs) { return fs.truncate(path_range(path), offset); });
    report_if_error("truncate", rs, path_);
    return rs ? 0 : -rs.error().value();
}

/* static */
int FuseRunner::_fuse_mknod(const char *path_, mode_t mode, dev_t rdev)
{
    fs::path path = remove_root(path_);
    auto r = QUERY_FS([&] (auto& fs) -> net::awaitable<int> {
            co_await fs.mknod(path_range(path), mode, rdev);
            co_return 0;
        });
    report_if_error("truncate", r, path_);
    return r ? 0 : -r.error().value();
}

/* static */
int FuseRunner::_fuse_mkdir(const char* path_, mode_t mode)
{
    fs::path path = remove_root(path_);
    auto r = QUERY_FS([&] (auto& fs) -> net::awaitable<int> {
        co_await fs.mkdir(path_range(path), mode);
        co_return 0;
    });
    report_if_error("mkdir", r, path_);
    return r ? 0 : -r.error().value();
}

/* static */
int FuseRunner::_fuse_utime(const char *path_, utimbuf* b)
{
    // TODO
    // struct utimbuf {
    //     time_t actime;       /* access time */
    //     time_t modtime;      /* modification time */
    // };
    return 0;
}

/* static */
int FuseRunner::_fuse_unlink(const char* path_)
{
    fs::path path = remove_root(path_);
    auto r = QUERY_FS([&] (auto& fs) -> net::awaitable<int> {
        co_await fs.remove_file(path_range(path));
        co_return 0;
    });
    report_if_error("unlink", r, path_);
    return r ? 0 : -r.error().value();
}

/* static */
int FuseRunner::_fuse_rmdir(const char* path_)
{
    fs::path path = remove_root(path_);
    auto r = QUERY_FS([&] (auto& fs) -> net::awaitable<int> {
        co_await fs.remove_directory(path_range(path));
        co_return 0;
    });
    report_if_error("rmdir", r, path_);
    return r ? 0 : -r.error().value();
}

void FuseRunner::run_loop()
{
    int err = fuse_loop(_fuse);
    if (err) throw std::runtime_error("FUSE: Session loop returned error");
}

void FuseRunner::finish()
{
    if (!_fuse_channel) return;
    auto c = _fuse_channel;
    _fuse_channel = nullptr;
    fuse_unmount(_mountdir.c_str(), c);
    _work.reset();
}

FuseRunner::~FuseRunner() {
    finish();
    _thread.join();
    if (_fuse) fuse_destroy(_fuse);
}
