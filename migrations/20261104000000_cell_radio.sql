-- LTE channel (EARFCN) and physical cell ID (PCI) as reported by Apple; null when unknown.
-- Cells on different channels don't compete for the same phone, so coverage models need them.
alter table cell add column earfcn integer;
alter table cell add column pci integer;
