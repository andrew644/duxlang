# The adventure project: a vendored module and the machine's zlib.
[package]
name = "adventure"
version = "0.1.0"
dux = "0.1"

[deps]
greetings = "../vendor/greetings"

[cdeps]
compression = "zlib"
