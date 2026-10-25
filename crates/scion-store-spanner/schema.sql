CREATE TABLE ScionRecords (
  Namespace STRING(1024) NOT NULL,
  RecordKey STRING(4096) NOT NULL,
  ValueBytes BYTES(MAX) NOT NULL
) PRIMARY KEY (Namespace, RecordKey);

CREATE TABLE ScionLogCounters (
  Repository STRING(256) NOT NULL,
  LastSequence INT64 NOT NULL
) PRIMARY KEY (Repository);

CREATE TABLE ScionLog (
  Repository STRING(256) NOT NULL,
  SequenceNumber INT64 NOT NULL,
  ChangesetId STRING(64) NOT NULL
) PRIMARY KEY (Repository, SequenceNumber);

CREATE TABLE ScionPublished (
  Repository STRING(256) NOT NULL,
  ChangesetId STRING(64) NOT NULL,
  SequenceNumber INT64 NOT NULL
) PRIMARY KEY (Repository, ChangesetId);
