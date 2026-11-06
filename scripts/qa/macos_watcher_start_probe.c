// Measure batched native registration and teardown on disposable local paths.
#include <CoreServices/CoreServices.h>
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
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

static double elapsed_ms(struct timespec before, struct timespec after) {
    return (after.tv_sec - before.tv_sec) * 1000.0 +
           (after.tv_nsec - before.tv_nsec) / 1000000.0;
}

int main(void) {
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

    CFStringRef source_path = CFStringCreateWithCString(NULL, source, kCFStringEncodingUTF8);
    CFStringRef parent_path = CFStringCreateWithCString(NULL, parent, kCFStringEncodingUTF8);
    assert(source_path != NULL && parent_path != NULL);
    const void *items[] = {source_path, parent_path};
    CFArrayRef paths = CFArrayCreate(NULL, items, 2, &kCFTypeArrayCallBacks);
    assert(paths != NULL);
    FSEventStreamRef stream = FSEventStreamCreate(
        NULL, observed, NULL, paths, kFSEventStreamEventIdSinceNow, 0.01,
        kFSEventStreamCreateFlagFileEvents | kFSEventStreamCreateFlagWatchRoot);
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
        FSEventStreamStop(stream);
    }
    FSEventStreamInvalidate(stream);
    FSEventStreamRelease(stream);
    clock_gettime(CLOCK_MONOTONIC, &stopped);
    CFRelease(paths);
    CFRelease(parent_path);
    CFRelease(source_path);
    if (rmdir(source) != 0 || rmdir(parent) != 0) {
        perror("remove disposable watch roots");
        return 1;
    }
    printf("paths=2 ready=%u start_ms=%.3f retire_ms=%.3f\n", (unsigned)ready,
           elapsed_ms(before, started), elapsed_ms(started, stopped));
    return ready ? 0 : 1;
}
