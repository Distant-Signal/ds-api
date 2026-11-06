# Reference data for the api: sources and terms

The code in this repository is licensed AGPL-3.0-or-later (`LICENSE`).
Some files here are derived from upstream data, which stays under its own
terms. This file lists each file's source and terms (repo split spec §6,
"Data is separate from code"). The site credits the same sources on
`/attribution` (`frontend/components/OpenDataAttribution.tsx`). The other
reference data files, and their terms, are listed in `reference-data/README.md`.

| File | Source | Terms |
|---|---|---|
| `fixed-link-operators.csv` | Seeded from production's CIF fixed links (2026-10-31), curated for Distant Signal. | AGPL-3.0-or-later; the codes are the CIF's ("Source: RSP"). |
| `operator-groups.csv`, `operator-names.csv` | Written for Distant Signal; operator codes as the timetable and the Knowledgebase use them. | AGPL-3.0-or-later. |
| `station-groups.csv` | Written for Distant Signal; the London Terminals list of the National Rail Routeing Guide. | AGPL-3.0-or-later (a list of station codes; no Routeing Guide text). |

The api's tests also read a recorded copy of GOV.UK's bank holidays JSON,
`crates/api/tests/fixtures/gov-uk-bank-holidays-2026-11-06.json` (fetched
from `https://www.gov.uk/bank-holidays.json` on 2026-11-06; Open Government
Licence v3.0, contains public sector information licensed under it). The
running api fetches the same file (`bank_holidays_sync`).
