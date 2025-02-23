# dlm-rust
A simple Download manager in rust

### Usage: 

```
dlm [OPTIONS] -x n -s n -k -d <output-dir> <url>
 
  <url>                   URL to download  

  -x, --connections <NUM>  Number of concurrent connections [default: 4]  
  -s, --chunk-size <SIZE>  Chunk size in bytes (minimum 16KB) [default: 1048576]  
  -k, --keep-partial       Keep partial downloads on failure  
  -d, --dir <DIR>          Download directory [default: .]  
  -h, --help               Print help  
  -V, --version            Print version
```
