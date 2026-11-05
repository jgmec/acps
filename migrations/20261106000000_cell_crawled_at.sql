-- When the cell was last crawled, i.e. returned in one of Apple's replies (refreshed at most
-- hourly). Cells crawled before this migration keep null: when they were crawled isn't
-- known. The default also stamps cells inserted by builds that don't set the column.
alter table cell add column crawled_at timestamp with time zone;
alter table cell alter column crawled_at set default current_timestamp;

create index cell_crawled_at on cell (crawled_at);
