# Test fixtures

Real bank notification emails, used by the parser tests in `src/parsers/`.

Before adding one, redact everything personal except the fields the parser
reads: replace names, card digits (use `1234`), reference numbers, PANs and
tracking links with fake values. Keep amounts, merchants, dates, times and the
surrounding labels exactly as the bank wrote them.

| File | Bank / template | Source |
| --- | --- | --- |
| `mandiri_livin/qris_credit_card.txt` | Livin' by Mandiri, QRIS paid by credit card (`noreply.livin@bankmandiri.co.id`, subject "Pembayaran Berhasil!") | Text copied from Gmail, Oct 2026 |
| `mandiri_card/charge.txt` | Mandiri Kartu Kredit card transaction (`mandiricare.onlinenotif@bankmandiri.co.id`) | Text copied from Gmail, Oct 2026 |
| `cimb_niaga/charge.txt` | CIMB Niaga credit card transaction, All Accor World card (`CREDITCARD.NOTIFICATION@cimbniaga.co.id`) | Text copied from Gmail, Oct 2026; no parser, read by the model fallback (Phase 2) |
| `jenius/charge.txt` | Jenius Credit Card transaction (`jenius_noreply@smbci.com`) | Text copied from Gmail, Oct 2026; no parser, read by the model fallback (Phase 2) |
