# mini-loader

The mini-loader is capable of loading and executing ELF files, including `Executable file` and `Position-Independent Executable file`

## Note
Currently only support `x86_64` .

## Installation
```shell
$ cargo install mini-loader --target x86_64-unknown-none
```

## Usage
Load and execute `ls`:

```shell
$ mini-loader /bin/ls
``` 