# Fixture provenance (battleground feeds)

Payloads recorded from public results sites and trimmed for size. Row content is byte-for-byte
what the site served; only whole rows (or whole JSON entries) were dropped. Fetched with a browser
User-Agent from a residential connection.

| Fixture | Source URL | Fetched | Trimmed to |
|---|---|---|---|
| `ncsbe_2024_results_pct_trimmed.zip` | https://s3.amazonaws.com/dl.ncsbe.gov/ENRS/2024_11_05/results_pct_20241105.zip (S3 `Last-Modified: Tue, 03 Dec 2024 18:32:58 GMT`, the certified file) | 2026-10-06 | Hyde, New Hanover and Watauga counties: every `US PRESIDENT` and `NC GOVERNOR` row, plus the first 15 rows of other contests per county (to exercise skipping). Entry name, tab separators and CRLF kept. |
| `ncsbe_2026_results_pct_template_trimmed.zip` | https://s3.amazonaws.com/dl.ncsbe.gov/ENRS/2026_11_03/results_pct_20261103.zip (S3 `LastModified: 2026-10-05T14:57:45Z`; all counts zero before the election) | 2026-10-06 | Hyde and New Hanover: every `US SENATE` row plus the first 5 other rows per county. |

Older fixtures are described in the module docs of `tests/adapters.rs` and `tests/enhanced_voting.rs`.
