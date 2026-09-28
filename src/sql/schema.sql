-- Local (client) schema, version 1. Applied by db::migrate.
CREATE TABLE task (
    id      INTEGER PRIMARY KEY                   ,
    uuid    TEXT                NOT NULL UNIQUE   , -- stable across devices
    title   TEXT                NOT NULL          ,
    done    INTEGER             NOT NULL DEFAULT 0,
    due     INTEGER                               , -- unix seconds
    created INTEGER             NOT NULL          , -- unix millis
    updated INTEGER             NOT NULL          , -- unix millis, for LWW
    deleted INTEGER             NOT NULL DEFAULT 0, -- sync tombstone
    dirty   INTEGER             NOT NULL DEFAULT 1  -- not yet pushed
);

CREATE TABLE meta (
    key     TEXT                PRIMARY KEY       ,
    value   TEXT                NOT NULL
);
