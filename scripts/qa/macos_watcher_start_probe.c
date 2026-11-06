// Measure batched native registration and teardown on disposable local paths.
#include <CoreServices/CoreServices.h>
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

static void observed(ConstFSEventStreamRef stream, void *context, size_t count,
                     void *paths, const FSEventStreamEventFlags flags[],
                     const FSEventStreamEventId ids[]) {
    (void)stream;
    (void)context;
    (void)count;
    (void)paths;
    (void)flags;
    (void)ids;
}

static void report_fixture_event(ConstFSEventStreamRef stream, void *context, size_t count,
                                 void *paths, const FSEventStreamEventFlags flags[],
                                 const FSEventStreamEventId ids[]) {
    (void)stream;
    (void)ids;
    const char *source = context;
    size_t source_len = strlen(source);
    const char *const *names = paths;
    for (size_t i = 0; i < count; i++) {
        const char *name = names[i];
        if (strcmp(name, source) == 0 && (flags[i] & kFSEventStreamEventFlagRootChanged)) {
            puts("root-changed");
        } else if (strncmp(name, source, source_len) == 0 && name[source_len] == '/' &&
                   strcmp(name + source_len, "/descendant/kick.wav") == 0) {
            puts("descendant");
        } else {
            const char *leaf = strrchr(name, '/');
            if (leaf != NULL && strcmp(leaf, "/unrelated.wav") == 0) {
                // Acknowledge receipt for the probe without delivering a Source hint.
                puts("filtered-sibling");
            }
        }
    }
}

static double elapsed_ms(struct timespec before, struct timespec after) {
    return (after.tv_sec - before.tv_sec) * 1000.0 +
           (after.tv_nsec - before.tv_nsec) / 1000000.0;
}

static int probe(char *source, const char *parent, int observe_events) {
    CFStringRef source_path = CFStringCreateWithCString(NULL, source, kCFStringEncodingUTF8);
    CFStringRef parent_path = CFStringCreateWithCString(NULL, parent, kCFStringEncodingUTF8);
    assert(source_path != NULL && parent_path != NULL);
    const void *items[] = {source_path, parent_path};
    CFArrayRef paths = CFArrayCreate(NULL, items, 2, &kCFTypeArrayCallBacks);
    assert(paths != NULL);
    FSEventStreamContext context = {0, source, NULL, NULL, NULL};
    FSEventStreamRef stream = FSEventStreamCreate(
        NULL, observe_events ? report_fixture_event : observed, &context, paths,
        kFSEventStreamEventIdSinceNow, 0.01, kFSEventStreamCreateFlagFileEvents |
            kFSEventStreamCreateFlagWatchRoot | kFSEventStreamCreateFlagNoDefer);
    assert(stream != NULL);
    CFArrayRef configured = FSEventStreamCopyPathsBeingWatched(stream);
    assert(configured != NULL && CFEqual(paths, configured));
    CFRelease(configured);

    // Use the same run-loop scheduling API as the pinned watcher backend.
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
    FSEventStreamScheduleWithRunLoop(stream, CFRunLoopGetCurrent(), kCFRunLoopDefaultMode);
#pragma clang diagnostic pop
    struct timespec before, started, stopped;
    clock_gettime(CLOCK_MONOTONIC, &before);
    Boolean ready = FSEventStreamStart(stream);
    clock_gettime(CLOCK_MONOTONIC, &started);
    if (ready) {
        if (observe_events) {
            puts("ready");
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, 5.0, false);
        }
        FSEventStreamStop(stream);
    }
    FSEventStreamInvalidate(stream);
    FSEventStreamRelease(stream);
    clock_gettime(CLOCK_MONOTONIC, &stopped);
    CFRelease(paths);
    CFRelease(parent_path);
    CFRelease(source_path);
    if (!observe_events) {
        printf("paths=2 ready=%u start_ms=%.3f retire_ms=%.3f\n", (unsigned)ready,
               elapsed_ms(before, started), elapsed_ms(started, stopped));
    }
    return ready ? 0 : 1;
}

int main(int argc, char **argv) {
    if (argc == 4 && strcmp(argv[1], "--observe") == 0) {
        setvbuf(stdout, NULL, _IONBF, 0);
        return probe(argv[3], argv[2], 1);
    }
    if (argc != 1) {
        return 2;
    }
    char parent[] = "/private/tmp/yield-watcher-start.XXXXXX";
    if (mkdtemp(parent) == NULL) {
        perror("mkdtemp");
        return 1;
    }
    char source[sizeof(parent) + sizeof("/source")];
    snprintf(source, sizeof(source), "%s/source", parent);
    if (mkdir(source, 0700) != 0) {
        perror("mkdir");
        rmdir(parent);
        return 1;
    }
    int status = probe(source, parent, 0);
    if (rmdir(source) != 0 || rmdir(parent) != 0) {
        perror("remove disposable watch roots");
        return 1;
    }
    return status;
}
