# Fixture provenance (battleground feeds)

Payloads recorded from public results sites and trimmed for size. Every value is what the site
served; only whole rows (or whole JSON entries) were dropped, and the table says where a file was
re-serialized after trimming. Fetched with a browser User-Agent.

| Fixture | Source URL | Fetched | Trimmed to |
|---|---|---|---|
| `ncsbe_2024_results_pct_trimmed.zip` | https://s3.amazonaws.com/dl.ncsbe.gov/ENRS/2024_11_05/results_pct_20241105.zip (S3 `Last-Modified: Tue, 03 Dec 2024 18:32:58 GMT`, the certified file) | 2026-10-06 | Hyde, New Hanover and Watauga counties: every `US PRESIDENT` and `NC GOVERNOR` row, plus the first 15 rows of other contests per county (to exercise skipping). Entry name, tab separators and CRLF kept. |
| `ncsbe_2026_results_pct_template_trimmed.zip` | https://s3.amazonaws.com/dl.ncsbe.gov/ENRS/2026_11_03/results_pct_20261103.zip (S3 `LastModified: 2026-10-05T14:57:45Z`; all counts zero before the election) | 2026-10-06 | Hyde and New Hanover: every `US SENATE` row plus the first 5 other rows per county. |
| `az_sos_2024_all_county_races_trimmed.json` | https://cdn1.arizona.vote/data/47/0/all_county_races_47_0_en_12036.json (AZ SoS 2024 general, eid 47, upload 12036; `Last-Modified: Fri, 22 Nov 2024 18:02:34 GMT`) | 2026-10-06 | Apache, Pima and Santa Cruz: every `President of the United States` and `U.S. Senator` row plus the first 3 other rows per county. Re-serialized compactly (same keys and values). |
| `az_sos_2024_election_47_0.json` | https://cdn1.arizona.vote/data/47/0/election_47_0.json | 2026-10-06 | Verbatim (the version document naming upload 12036). |

Older fixtures are described in the module docs of `tests/adapters.rs` and `tests/enhanced_voting.rs`.
