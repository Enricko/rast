-- Size limits of shared folders (schema v15, ADR 0005 decision D3).
--
-- NULL means no limit, which every existing shared folder keeps. A limit is
-- a positive number of bytes. RAST checks it for File Station writes; with
-- group quota on the shared filesystem the kernel also limits the share's
-- read-write group ("filesystem, best effort").
ALTER TABLE shares ADD COLUMN quota_bytes INTEGER
    CHECK (quota_bytes IS NULL OR quota_bytes > 0);
