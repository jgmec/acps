-- Altitude and vertical accuracy as reported by Apple, in metres; null when the reply has
-- none (so far Apple has only sent -1 for both, meaning unknown, on cells it didn't find).
alter table cell add column altitude bigint;
alter table cell add column vertical_acc bigint;
