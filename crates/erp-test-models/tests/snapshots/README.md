<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->

# Test snapshots

## `assistant_tool_schemas.json`

The canonical JSON (RFC 8785, `erp_fields::json::canonical`) of the assistant tool-schema export of
the test-model registry, followed by one newline: every exposed method with a closed parameter table
and its schema, the offered methods, and the `values`
schemas of `test_assistant.task` and `test_orm.mixed` (, `tests/assistant_tool_schema.rs`,
`tool_schemas_are_deterministic_and_pinned`).

The bytes are written by the exporter, never by hand. When the test models or the export change, the
test fails and writes the new export next to the snapshot as `assistant_tool_schemas.json.actual` on
the build host. Regenerate from the repository root (never on the laptop):

```sh
CI_HOST=a build host scripts/ci-focus.sh <slice> -p erp-test-models \
  -E 'test(=tool_schemas_are_deterministic_and_pinned)'
scp a build host:Projects/ERPForAll-<slice>/crates/erp-test-models/tests/snapshots/assistant_tool_schemas.json.actual \
  crates/erp-test-models/tests/snapshots/assistant_tool_schemas.json
```

Review the diff of the regenerated file, commit it, and run the test again: it must pass.
