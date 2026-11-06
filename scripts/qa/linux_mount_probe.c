/* Read-only Linux mount observation for the capacity authority investigation. */
#define _GNU_SOURCE
#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <inttypes.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef __linux__
#include <fcntl.h>
#include <linux/magic.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/vfs.h>
#include <unistd.h>

#ifndef STATX_MNT_ID_UNIQUE
#define STATX_MNT_ID_UNIQUE 0x00004000U
#endif
#endif

struct mount_record {
    uint64_t major;
    uint64_t minor;
    bool ext4_candidate;
};

enum row_result { ROW_INVALID, ROW_OTHER, ROW_MATCH };

static bool parse_uint(const char *value, uint64_t *out) {
    char *end;
    if (value == NULL || value[0] < '0' || value[0] > '9') {
        return false;
    }
    errno = 0;
    uintmax_t parsed = strtoumax(value, &end, 10);
    if (errno != 0 || *end != '\0' || parsed > UINT64_MAX) {
        return false;
    }
    *out = (uint64_t)parsed;
    return true;
}

static bool has_option(const char *options, const char *key) {
    size_t key_length = strlen(key);
    while (*options != '\0') {
        size_t length = strcspn(options, ",");
        if (length == key_length && strncmp(options, key, length) == 0) {
            return true;
        }
        options += length;
        if (*options == ',') {
            ++options;
        }
    }
    return false;
}

static bool writable(const char *options) {
    return has_option(options, "rw") && !has_option(options, "ro");
}

static enum row_result parse_row(char *line, uint64_t wanted, struct mount_record *out) {
    char *cursor;
    uint64_t id;
    if (!parse_uint(strtok_r(line, " \n", &cursor), &id)) {
        return ROW_INVALID;
    }
    if (id != wanted) {
        return ROW_OTHER;
    }
    char *parent = strtok_r(NULL, " \n", &cursor);
    char *device = strtok_r(NULL, " \n", &cursor);
    char *root = strtok_r(NULL, " \n", &cursor);
    char *point = strtok_r(NULL, " \n", &cursor);
    char *options = strtok_r(NULL, " \n", &cursor);
    uint64_t parent_id;
    char *colon = device == NULL ? NULL : strchr(device, ':');
    if (!parse_uint(parent, &parent_id) || colon == NULL || root == NULL || point == NULL ||
        options == NULL || parent_id == 0 || root[0] != '/' || point[0] != '/') {
        return ROW_INVALID;
    }
    *colon = '\0';
    if (!parse_uint(device, &out->major) || !parse_uint(colon + 1, &out->minor)) {
        return ROW_INVALID;
    }
    char *field;
    do {
        field = strtok_r(NULL, " \n", &cursor);
    } while (field != NULL && strcmp(field, "-") != 0);
    char *type = strtok_r(NULL, " \n", &cursor);
    char *source = strtok_r(NULL, " \n", &cursor);
    char *super_options = strtok_r(NULL, " \n", &cursor);
    if (field == NULL || type == NULL || source == NULL || super_options == NULL ||
        strtok_r(NULL, " \n", &cursor) != NULL) {
        return ROW_INVALID;
    }
    out->ext4_candidate =
        strcmp(type, "ext4") == 0 && out->major != 0 && strcmp(root, "/") == 0 &&
        strncmp(source, "/dev/", 5) == 0 && writable(options) && writable(super_options);
    return ROW_MATCH;
}

static bool read_mount(FILE *stream, uint64_t id, struct mount_record *out) {
    char line[64 * 1024];
    size_t bytes = 0;
    bool found = false;
    while (fgets(line, sizeof(line), stream) != NULL) {
        size_t length = strlen(line);
        bytes += length;
        if (length == 0 || line[length - 1] != '\n' || bytes > 1024 * 1024) {
            return false;
        }
        struct mount_record row;
        switch (parse_row(line, id, &row)) {
            case ROW_INVALID:
                return false;
            case ROW_OTHER:
                continue;
            case ROW_MATCH:
                if (found) {
                    return false;
                }
                *out = row;
                found = true;
        }
    }
    return found && !ferror(stream);
}

static bool same_device(const struct mount_record *record, uint64_t major, uint64_t minor) {
    return record->major == major && record->minor == minor;
}

static bool fixture(const char *text, bool expected_match, bool expected_candidate) {
    FILE *stream = tmpfile();
    if (stream == NULL) {
        perror("temporary parser fixture");
        return false;
    }
    bool written = fputs(text, stream) >= 0 && fseek(stream, 0, SEEK_SET) == 0;
    struct mount_record record;
    bool matched = written && read_mount(stream, 42, &record);
    fclose(stream);
    return written && matched == expected_match &&
           (!matched || (record.ext4_candidate == expected_candidate &&
                         same_device(&record, 8, 1) && !same_device(&record, 8, 2)));
}

static int self_test(void) {
    const char *ext4 = "42 20 8:1 / /mnt/space\\040root rw,nosuid shared:7 - ext4 /dev/sda1 rw\n";
    const char *overlay = "42 20 8:1 / /mnt rw - overlay overlay rw,lowerdir=/example\n";
    const char *readonly_mount = "42 20 8:1 / /mnt ro - ext4 /dev/sda1 rw\n";
    const char *readonly_fs = "42 20 8:1 / /mnt rw - ext4 /dev/sda1 ro\n";
    const char *bound_subdir = "42 20 8:1 /subdir /mnt rw - ext4 /dev/sda1 rw\n";
    const char *missing = "43 20 8:1 / /mnt rw - ext4 /dev/sda1 rw\n";
    const char *malformed = "42 20 8:1 / /mnt rw ext4 /dev/sda1 rw\n";
    const char *duplicate = "42 20 8:1 / /mnt rw - ext4 /dev/sda1 rw\n"
                            "42 20 8:1 / /mnt rw - ext4 /dev/sda1 rw\n";
    const char *truncated = "42 20 8:1 / /mnt rw - ext4 /dev/sda1 rw";
    if (!fixture(ext4, true, true) || !fixture(overlay, true, false) ||
        !fixture(readonly_mount, true, false) || !fixture(readonly_fs, true, false) ||
        !fixture(bound_subdir, true, false) || !fixture(missing, false, false) ||
        !fixture(malformed, false, false) || !fixture(duplicate, false, false) ||
        !fixture(truncated, false, false)) {
        fputs("Mount parser fixture failed\n", stderr);
        return 1;
    }
    puts("Nine mount parser fixtures passed");
    return 0;
}

#ifdef __linux__
static bool descriptor_stat(int fd, unsigned int mount_mask, struct statx *facts) {
    const unsigned int required = STATX_TYPE | STATX_INO | mount_mask;
    return statx(fd, "", AT_EMPTY_PATH | AT_STATX_DONT_SYNC, required, facts) == 0 &&
           (facts->stx_mask & required) == required && facts->stx_mnt_id != 0;
}

static bool same_object(const struct statx *first, const struct statx *second) {
    return first->stx_ino == second->stx_ino && first->stx_dev_major == second->stx_dev_major &&
           first->stx_dev_minor == second->stx_dev_minor &&
           (first->stx_mode & S_IFMT) == (second->stx_mode & S_IFMT);
}

static int observe(int fd) {
    struct statx unique, legacy, after;
    struct statfs filesystem;
    if (!descriptor_stat(fd, STATX_MNT_ID_UNIQUE, &unique) ||
        !descriptor_stat(fd, STATX_MNT_ID, &legacy) || !same_object(&unique, &legacy) ||
        (!S_ISREG(unique.stx_mode) && !S_ISDIR(unique.stx_mode)) ||
        fstatfs(fd, &filesystem) != 0) {
        fputs("Refused because the descriptor or required kernel mount facts are unavailable\n", stderr);
        return 2;
    }

    struct mount_record mount;
    FILE *stream = fopen("/proc/self/mountinfo", "r");
    if (stream == NULL) {
        perror("open kernel mount information");
        return 2;
    }
    bool found = read_mount(stream, (uint64_t)legacy.stx_mnt_id, &mount);
    fclose(stream);
    if (!found || !same_device(&mount, unique.stx_dev_major, unique.stx_dev_minor) ||
        !descriptor_stat(fd, STATX_MNT_ID_UNIQUE, &after) || !same_object(&unique, &after) ||
        unique.stx_mnt_id != after.stx_mnt_id) {
        fputs("Refused because the retained descriptor and mount observation did not agree\n", stderr);
        return 2;
    }

    bool candidate = mount.ext4_candidate && filesystem.f_type == EXT4_SUPER_MAGIC &&
                     (filesystem.f_flags & ST_RDONLY) == 0;
    printf("mount=%" PRIu64 " legacy=%" PRIu64 " device=%" PRIu32 ":%" PRIu32
           " fs_magic=%#lx writable_ext4_candidate=%s\n",
           (uint64_t)unique.stx_mnt_id, (uint64_t)legacy.stx_mnt_id,
           unique.stx_dev_major, unique.stx_dev_minor, (unsigned long)filesystem.f_type,
           candidate ? "yes" : "no");
    return candidate ? 0 : 2;
}
#endif

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "--self-test") == 0) {
        return self_test();
    }
    if (argc != 2) {
        fputs("Usage: linux_mount_probe <file-or-directory> | --self-test\n", stderr);
        return 1;
    }
#ifdef __linux__
    int fd = open(argv[1], O_PATH | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) {
        perror("open probe descriptor");
        return 2;
    }
    int result = observe(fd);
    close(fd);
    return result;
#else
    fputs("Descriptor observation requires Linux\n", stderr);
    return 2;
#endif
}
