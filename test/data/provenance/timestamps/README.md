# Public RFC 3161 interoperability fixtures

These are generated test certificates and signed data, not operational credentials.
`response.tsr` is an actual OpenSSL 3.5 timestamp response to `request.tsq` over the
complete canonical bytes in `envelope.json` (including its complete version-2 CMS objects).
The envelope comes from the shared signing fixture; this folder tests timestamp
integrity and separately tests historical certificate eligibility, not a production
trust decision for the signing fixture's demo certificates.

`metadata.json` fixes the TSA generation time, authorized test policy OID and the
later relying-party verification time. The TSA and historical signer certificates
expire between those times. `tsa-root.crl.pem` is valid at the timestamp time;
`stale.crl.pem` is already expired then. Test keys were generated transiently and
were not retained. The test root and policy OID must never be used in production.
