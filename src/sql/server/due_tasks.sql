-- Open tasks whose reminder time has come by ?1, using each task's own
-- lead time (?2 minutes when it has none; off counts as 0). Also returns
-- what the notifier last sent for each.
SELECT
    t.uuid  ,
    t.title ,
    t.due   ,
    t.remind,
    t.nag   ,
    n.due   ,
    n.last
FROM
    task t
    LEFT JOIN notified n ON n.uuid = t.uuid
WHERE
    t.done = 0
    AND t.deleted = 0
    AND t.due IS NOT NULL
    AND t.due - 60 * (
        CASE
            WHEN t.remind IS NULL THEN ?2
            WHEN t.remind < 0     THEN 0
            ELSE t.remind
        END
    ) <= ?1
ORDER BY
    t.due
;
