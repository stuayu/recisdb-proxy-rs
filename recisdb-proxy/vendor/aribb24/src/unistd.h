
// vendor/aribb24/src/unistd.h
#pragma once

#ifdef _WIN32
  #include <io.h>
  #include <direct.h>
  #include <stdint.h>
  #include <stddef.h>
  #include <sys/stat.h>

  // bits.h が ssize_t を必要とするため定義 [3](https://github.com/xtne6f/bondump/blob/master/IBonDriver2.h)
  typedef intptr_t ssize_t;

  // POSIX 風API名を MSVC の _xxx に寄せる（drcs.c 側が使う）[1](https://github.com/u-n-k-n-o-w-n/BonDriverProxy_Linux)
  #define access  _access
  #define open    _open
  #define close   _close
  #define read    _read
  #define write   _write
  #define unlink  _unlink

  // ★ mkdir は定義しない（drcs.c 側で mkdir(a,b) → mkdir(a) を既に処理しているため）[1](https://github.com/u-n-k-n-o-w-n/BonDriverProxy_Linux)

  #ifndef F_OK
    #define F_OK 0
  #endif

  // ---- POSIX パーミッション互換（MSVC で未定義になるため） ----
  // drcs.c が S_IRUSR/S_IWUSR を参照して落ちている [1](https://github.com/u-n-k-n-o-w-n/BonDriverProxy_Linux)
  #ifndef S_IRUSR
    #define S_IRUSR _S_IREAD
  #endif
  #ifndef S_IWUSR
    #define S_IWUSR _S_IWRITE
  #endif
  #ifndef S_IXUSR
    #define S_IXUSR 0
  #endif
  #ifndef S_IRWXU
    #define S_IRWXU (S_IRUSR | S_IWUSR | S_IXUSR)
  #endif

  // ついでに、未定義が出ることが多いので用意（必要なければ害なし）
  #ifndef S_IRGRP
    #define S_IRGRP 0
  #endif
  #ifndef S_IWGRP
    #define S_IWGRP 0
  #endif
  #ifndef S_IXGRP
    #define S_IXGRP 0
  #endif
  #ifndef S_IROTH
    #define S_IROTH 0
  #endif
  #ifndef S_IWOTH
    #define S_IWOTH 0
  #endif
  #ifndef S_IXOTH
    #define S_IXOTH 0
  #endif
  #ifndef S_IRWXG
    #define S_IRWXG (S_IRGRP | S_IWGRP | S_IXGRP)
  #endif
  #ifndef S_IRWXO
    #define S_IRWXO (S_IROTH | S_IWOTH | S_IXOTH)
  #endif
#endif
