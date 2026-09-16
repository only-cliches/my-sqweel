# Additional complete upstream cases

The coverage worker places reviewed-candidate `.txt` manifests here, one complete
MTR file per entry with test and result SHA-256 hashes. CI combines these with the
existing strict manifest for execution. Files are not fragments or generated SQL.
An upstream case qualifies only after both baseline and MySqweel CI gates pass.
