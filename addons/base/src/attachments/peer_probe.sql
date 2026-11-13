-- SPDX-License-Identifier: MIT OR Apache-2.0
-- One current attempt per store. Installation never invents successful evidence.
CREATE TABLE IF NOT EXISTS ir_blob_peer_probe (
    store_id int2 PRIMARY KEY REFERENCES ir_blob_store(id) ON DELETE RESTRICT,
    session uuid NOT NULL CHECK(session <> '00000000-0000-0000-0000-000000000000'),
    namespace uuid NOT NULL, fingerprint text NOT NULL CHECK(octet_length(fingerprint) BETWEEN 1 AND 128),
    incarnation uuid NOT NULL, protocol int2 NOT NULL CHECK(protocol = 1),
    node_a text NOT NULL CHECK(octet_length(node_a) BETWEEN 1 AND 63),
    host_a text NOT NULL CHECK(octet_length(host_a) BETWEEN 1 AND 128),
    mount_a text NOT NULL CHECK(mount_a ~ '^[0-9a-f]{64}$'),
    node_b text CHECK(octet_length(node_b) BETWEEN 1 AND 63),
    host_b text CHECK(octet_length(host_b) BETWEEN 1 AND 128),
    mount_b text CHECK(mount_b ~ '^[0-9a-f]{64}$'),
    a bytea NOT NULL DEFAULT '' CHECK(octet_length(a) <= 16),
    b bytea NOT NULL DEFAULT '' CHECK(octet_length(b) <= 16),
    status text NOT NULL CHECK(status IN ('pending','proved','verified','failed')),
    deadline timestamptz NOT NULL,
    CHECK((node_b IS NULL) = (host_b IS NULL) AND (node_b IS NULL) = (mount_b IS NULL)),
    CHECK(node_b IS NULL OR (node_a <> node_b AND host_a <> host_b)),
    CHECK(status NOT IN ('proved','verified') OR (node_b IS NOT NULL AND octet_length(a)=16 AND octet_length(b)=16))
);
REVOKE ALL ON ir_blob_peer_probe FROM PUBLIC;
