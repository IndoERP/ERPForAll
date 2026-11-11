# Independent installed catalog expectations

These facts are independent of the addon seed files. The installed acceptance test reads the actual database rows
after the real base manifest runs and compares them to `currencies.json`, `languages.json` and
`original-identities.json`.

Do not regenerate the expectations to make a failing assertion pass: first decide whether the seed data or the
expectation is wrong.
