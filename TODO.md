# TODO

* The durable_batch impl isn't using preloading correctly. That's the next job. It might make sense to write a new class which exposes a KV interface + a pre_advise so it can be tested independently.
* The hash chain component isn't implemented
* Lookup / history proofs aren't implemented.
* Versioning isn't implemented
* Witnessing isn't implemented
* There's no web api yet.