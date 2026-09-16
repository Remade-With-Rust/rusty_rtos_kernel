# SBOM

CycloneDX software bills of materials for this repository's published crates,
one per crate, regenerated with:

```sh
cargo cyclonedx --format json --all
```

They live **here** rather than inside each crate directory on purpose: an SBOM
shipped inside the crate it describes is stale the moment a dependency moves,
and it would be published as part of the package. These are release artefacts.

Hardening gate H-12.
