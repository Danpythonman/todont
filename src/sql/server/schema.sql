-- Server schema, version 1. Applied by server::open_db.
CREATE TABLE task (
    uuid    TEXT                PRIMARY KEY       ,
    title   TEXT                NOT NULL          ,
    done    INTEGER             NOT NULL          ,
    due     INTEGER                               , -- unix seconds
    created INTEGER             NOT NULL          , -- unix millis
    updated INTEGER             NOT NULL          , -- unix millis, for LWW
    deleted INTEGER             NOT NULL          ,
    seq     INTEGER             NOT NULL            -- server change order
);
CREATE INDEX task_seq ON task (seq);

-- What the notifier has already sent, per task. Not synced.
CREATE TABLE notified (
    uuid    TEXT                PRIMARY KEY       ,
    due     INTEGER             NOT NULL          , -- due time reminded for
    last    INTEGER             NOT NULL            -- unix seconds
);

CREATE TABLE meta (
    key     TEXT                PRIMARY KEY       ,
    value   TEXT                NOT NULL
);
