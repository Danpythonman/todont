-- Version 2: per-task notification settings, decoded by alerts.rs.
ALTER TABLE task ADD COLUMN remind INTEGER; -- mins before due; NULL default, -1 off
ALTER TABLE task ADD COLUMN nag    INTEGER; -- mins between;    NULL default,  0 off
