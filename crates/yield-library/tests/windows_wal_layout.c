/* Compile-only GNU Windows ABI check against the actual locked SQLite source.
 * Supply -DYIELD_SQLITE_SOURCE='"/absolute/path/to/sqlite3.c"'.
 * This does not execute Windows APIs or qualify allocation or durability.
 */
#include <stddef.h>
#include <windows.h>
#include <winternl.h>
#ifndef YIELD_SQLITE_SOURCE
#error "Supply the actual locked libsqlite3-sys bundled sqlite3.c"
#endif
#include YIELD_SQLITE_SOURCE

_Static_assert(SQLITE_VERSION_NUMBER == 3050002, "reviewed SQLite version");
_Static_assert(sizeof(void *) == 8, "reviewed 64-bit Windows layout");
_Static_assert(offsetof(winFile, h) == 16, "original winFile handle");
_Static_assert(offsetof(winFile, pShm) == 40, "original winFile SHM");
_Static_assert(offsetof(winShm, hShm) == 16, "original SHM locking handle");
_Static_assert(offsetof(winShm, bReadonly) == 24, "original SHM mode");
_Static_assert(offsetof(winShmNode, hSharedShm) == 16, "original shared mapping file");
_Static_assert(offsetof(winShmNode, isUnlocked) == 24, "original DMS state");
_Static_assert(offsetof(winShmNode, isReadonly) == 28, "original shared mode");
_Static_assert(offsetof(winShmNode, szRegion) == 32, "original region size");
_Static_assert(offsetof(winShmNode, nRegion) == 36, "original region count");
_Static_assert(offsetof(winShmNode, aRegion) == 40, "original region array");
_Static_assert(sizeof(struct ShmRegion) == 16, "original region size");
_Static_assert(offsetof(struct ShmRegion, pMap) == 8, "original mapped address");

_Static_assert(sizeof(FILE_FS_FULL_SIZE_INFORMATION) == 32, "native volume record size");
_Static_assert(offsetof(FILE_FS_FULL_SIZE_INFORMATION, TotalAllocationUnits) == 0, "total count");
_Static_assert(offsetof(FILE_FS_FULL_SIZE_INFORMATION, CallerAvailableAllocationUnits) == 8, "caller count");
_Static_assert(offsetof(FILE_FS_FULL_SIZE_INFORMATION, ActualAvailableAllocationUnits) == 16, "actual count");
_Static_assert(offsetof(FILE_FS_FULL_SIZE_INFORMATION, SectorsPerAllocationUnit) == 24, "sector count");
_Static_assert(offsetof(FILE_FS_FULL_SIZE_INFORMATION, BytesPerSector) == 28, "sector size");
