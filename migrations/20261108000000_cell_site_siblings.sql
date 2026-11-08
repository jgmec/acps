-- when a site's own missing sector numbers (next to its known ones) have been guessed;
-- probed_at stays for the guesses at neighbouring site numbers
alter table cell_site add column siblings_probed_at timestamp with time zone;

create index cell_site_siblings_todo on cell_site (mcc, mnc, site) where siblings_probed_at is null;
