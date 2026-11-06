// OPT-1869: disposable macOS syscall probe. Never run against user files.
// cc -Wall -Wextra -Werror scripts/qa/macos_publication_probe.c -o /tmp/yield-publication-probe
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/clonefile.h>
#include <sys/stat.h>
#include <unistd.h>

static void check(int result, const char *operation) {
    if (result == -1) {
        perror(operation);
        exit(1);
    }
}

static void write_entry(int dir, const char *name, const char *bytes) {
    int fd = openat(dir, name, O_CREAT | O_EXCL | O_WRONLY | O_CLOEXEC, 0600);
    check(fd, "openat create");
    size_t length = strlen(bytes);
    assert(write(fd, bytes, length) == (ssize_t)length);
    check(close(fd), "close created file");
}

static void expect_entry(int dir, const char *name, const char *bytes) {
    int fd = openat(dir, name, O_RDONLY | O_NOFOLLOW | O_CLOEXEC);
    check(fd, "openat read");
    char buffer[128] = {0};
    ssize_t count = read(fd, buffer, sizeof(buffer) - 1);
    check((int)count, "read entry");
    assert(strcmp(buffer, bytes) == 0);
    check(close(fd), "close read file");
}

int main(void) {
    char fixture[] = "/private/tmp/yield-publication-probe.XXXXXX";
    if (mkdtemp(fixture) == NULL) {
        perror("mkdtemp");
        return 1;
    }
    int dir = open(fixture, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    check(dir, "open fixture directory");

    // A held directory fd stays on the original directory after its name moves.
    check(mkdirat(dir, "root", 0700), "mkdir root");
    int root = openat(dir, "root", O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    check(root, "open root");
    check(renameat(dir, "root", dir, "moved-root"), "move root");
    check(mkdirat(dir, "root", 0700), "replace root name");
    write_entry(root, "anchor", "held-root");
    int new_root = openat(dir, "root", O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    check(new_root, "open replacement root");
    assert(faccessat(new_root, "anchor", F_OK, 0) == -1 && errno == ENOENT);
    puts("root replacement: held dirfd remained on old root");

    // RENAME_EXCL protects an existing final, but its source is a mutable name.
    write_entry(root, "stage", "owned");
    int held = openat(root, "stage", O_RDONLY | O_NOFOLLOW | O_CLOEXEC);
    check(held, "open owned stage");
    check(renameat(root, "stage", root, "held-stage"), "substitute stage name");
    write_entry(root, "stage", "unrelated");
    check(renameatx_np(root, "stage", root, "final", RENAME_EXCL), "exclusive rename");
    expect_entry(root, "final", "unrelated");
    expect_entry(root, "held-stage", "owned");
    puts("renameatx_np: source-name substitution published unrelated bytes");

    write_entry(root, "collision-stage", "owned");
    write_entry(root, "collision-final", "unrelated");
    errno = 0;
    assert(renameatx_np(root, "collision-stage", root, "collision-final", RENAME_EXCL) == -1);
    assert(errno == EEXIST);
    expect_entry(root, "collision-stage", "owned");
    expect_entry(root, "collision-final", "unrelated");
    puts("renameatx_np: late destination collision preserved both entries");

    // fclonefileat binds the held source, and creates a fresh destination.
    int cloned = fclonefileat(held, root, "clone-final", 0);
    if (cloned == -1 && errno == ENOTSUP) {
        puts("fclonefileat: filesystem does not support cloning");
    } else {
        check(cloned, "fclonefileat");
        expect_entry(root, "clone-final", "owned");
        errno = 0;
        assert(fclonefileat(held, root, "collision-final", 0) == -1);
        assert(errno == EEXIST);
        expect_entry(root, "collision-final", "unrelated");
        check(renameat(root, "clone-final", root, "original-clone"), "replace cloned name");
        write_entry(root, "clone-final", "unrelated");
        expect_entry(root, "clone-final", "unrelated");
        expect_entry(root, "original-clone", "owned");
        puts("fclonefileat: source fd bound and collision safe; later final-name reopen saw substituted entry");
    }

    // unlinkat resolves a mutable name, even when the owned file remains open.
    write_entry(root, "cleanup-stage", "owned");
    int cleanup_held = openat(root, "cleanup-stage", O_RDONLY | O_NOFOLLOW | O_CLOEXEC);
    check(cleanup_held, "open cleanup stage");
    check(renameat(root, "cleanup-stage", root, "preserved-stage"), "substitute cleanup name");
    write_entry(root, "cleanup-stage", "unrelated");
    check(unlinkat(root, "cleanup-stage", 0), "unlink substituted stage");
    assert(faccessat(root, "cleanup-stage", F_OK, 0) == -1 && errno == ENOENT);
    expect_entry(root, "preserved-stage", "owned");
    puts("unlinkat: cleanup-name substitution deleted unrelated entry");

    // Exclusive create returns the owned fd but exposes the final name before completion.
    const int create_flags = O_CREAT | O_EXCL | O_WRONLY | O_NOFOLLOW | O_CLOEXEC;
    int output = openat(root, "direct-output", create_flags, 0600);
    check(output, "openat exclusive final create");
    expect_entry(root, "direct-output", "");
    assert(write(output, "part", 4) == 4);
    expect_entry(root, "direct-output", "part");
    assert(openat(root, "direct-output", create_flags, 0600) == -1 && errno == EEXIST);
    check(renameat(root, "direct-output", root, "original-output"), "substitute direct output");
    write_entry(root, "direct-output", "unrelated");
    assert(write(output, "ial", 3) == 3);
    check(close(output), "close direct output");
    expect_entry(root, "direct-output", "unrelated");
    expect_entry(root, "original-output", "partial");
    puts("openat: exclusive create held its own file but exposed empty and partial final data");

    const char *entries[] = {
        "anchor", "held-stage", "final", "collision-stage", "collision-final",
        "original-clone", "clone-final", "preserved-stage", "stage",
        "direct-output", "original-output"
    };
    for (size_t i = 0; i < sizeof(entries) / sizeof(entries[0]); ++i) {
        if (unlinkat(root, entries[i], 0) == -1 && errno != ENOENT) {
            perror("cleanup fixture entry");
            return 1;
        }
    }
    check(close(cleanup_held), "close cleanup held");
    check(close(held), "close stage held");
    check(close(new_root), "close replacement root");
    check(close(root), "close held root");
    check(close(dir), "close fixture");
    int parent = open(fixture, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    check(parent, "reopen fixture");
    check(unlinkat(parent, "root", AT_REMOVEDIR), "remove replacement root");
    check(unlinkat(parent, "moved-root", AT_REMOVEDIR), "remove held root");
    check(close(parent), "close fixture parent");
    check(rmdir(fixture), "remove fixture");
    puts("disposable fixture removed");
    return 0;
}
