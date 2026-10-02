-- Global library migration is column-based, not SQLite user_version-based.
CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE sources (id TEXT PRIMARY KEY, root TEXT NOT NULL, sort_order INTEGER NOT NULL);
INSERT INTO metadata VALUES ('ui_preferences_v1', '{"schema_version":1,"browser_zoom":1.25}');
