# simple_wal

A simple rust write-ahead-logging implementation.

Features
 - Optimized for sequential reads & writes
 - Easy atomic log compaction
 - Advisory locking
 - CRC32 checksums
 - Range scans
 - Persistent log entry index
