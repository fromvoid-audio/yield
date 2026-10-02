-- Known v11 source shape from source_db_migration_tests::contract.
PRAGMA user_version = 11;
CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO metadata VALUES ('revision', '7');
CREATE TABLE wav_files (
    path TEXT PRIMARY KEY,
    file_size INTEGER NOT NULL,
    modified_ns INTEGER NOT NULL
);
INSERT INTO wav_files VALUES ('legacy/one.wav', 42, 99);
