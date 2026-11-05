-- Undocumented location fields 11 and 12 from Apple's cell replies, stored to work out what
-- they mean. Field 11 is ~940 for real cells; field 12 varies widely and tracks accuracy.
alter table cell add column unknown_11 bigint;
alter table cell add column unknown_12 bigint;
