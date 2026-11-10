-- Radio technology of each cell, as Apple's request it came from: 'lte' (LTE, request field
-- 25) or 'gsm' (2G GSM and 3G UMTS, field 1). The two number their location areas (TAC,
-- LAC) and cells independently, so the same numbers can mean different cells; radio is part
-- of the key. Every existing row is LTE.
alter table cell add column radio text not null default 'lte';
alter table cell drop constraint cell_pkey, add primary key (mcc, mnc, tac_id, cell_id, radio);

alter table cell_todo add column radio text not null default 'lte';
alter table cell_todo drop constraint cell_todo_pkey, add primary key (mcc, mnc, tac_id, cell_id, radio);

alter table cell_not_found add column radio text not null default 'lte';
alter table cell_not_found drop constraint cell_not_found_pkey,
    add primary key (mcc, mnc, tac_id, cell_id, radio);

-- GSM/UMTS cell IDs scanned by the crawler, in blocks of 500 consecutive IDs within one LAC
-- (block = cell_id / 500). Apple's GSM replies contain only the asked cells, so the crawler
-- finds new ones by asking for whole blocks around known cells. (cell_probe stays LTE only.)
create table cell_gsm_block (
    mcc integer not null,
    mnc integer not null,
    lac integer not null,
    block bigint not null,
    found integer not null,
    scanned_at timestamp with time zone not null default current_timestamp,
    primary key (mcc, mnc, lac, block)
);
