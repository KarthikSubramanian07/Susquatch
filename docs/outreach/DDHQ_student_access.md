# Draft: Decision Desk HQ student data access (for Karthik to send; nothing is sent automatically)

**To:** DDHQ via the contact form at https://decisiondeskhq.com/ (or sales/API contact listed at https://votes-docs.decisiondeskhq.com/)
**Subject:** Student research access to election-night results API (2026 midterms)

Hi DDHQ team,

I'm a student building an open-source election-night modeling project for the 2026 midterms. It's a county-level
expected-remaining-vote model, similar in spirit to the method your team published in the Harvard Data Science Review.
The code will be public after the election.

Do you offer free or academic access to your results API (county-level results with vote-type/batch tagging) for
non-commercial student research on election night, Nov 3–4? I'd use it only for my own modeling, would never redistribute your data,
and would credit DDHQ in the write-up. If there's a free tier, sandbox, or historical archive sample (2022/2024)
available to students, that would also help a lot for testing.

Happy to share the finished write-up with your team.

Thanks,
Karthik Subramanian
<school, program, expected graduation>

---
*Notes for Karthik: only accept terms that cost $0 (OPS_FREE_AUTONOMY §A). If they say yes, add the credentials to `.env`
(never commit them) and tell me. I'll add a DDHQ adapter behind the same `ResultsSource` trait as the state feeds.
Don't mention the Predictions Cup specifically unless you're comfortable with that. Using DDHQ data is public
information, not MNPI, but read their terms for any restriction on use in contests.*
