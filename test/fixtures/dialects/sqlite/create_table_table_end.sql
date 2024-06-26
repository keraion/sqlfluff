CREATE TABLE foo (
    id INTEGER NOT NULL PRIMARY KEY
)
WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS wordcount(
  word TEXT PRIMARY KEY,
  cnt INTEGER
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS wordcount(
  word TEXT PRIMARY KEY,
  cnt INTEGER
) STRICT;

CREATE TABLE IF NOT EXISTS wordcount(
  word TEXT PRIMARY KEY,
  cnt INTEGER
) WITHOUT ROWID, STRICT;
