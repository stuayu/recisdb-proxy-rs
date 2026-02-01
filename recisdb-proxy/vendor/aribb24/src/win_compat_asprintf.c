
// vendor/aribb24/src/win_compat_asprintf.c
#ifdef _WIN32
  #include <stdarg.h>
  #include <stdio.h>
  #include <stdlib.h>

  int vasprintf(char **strp, const char *fmt, va_list ap)
  {
      if (!strp) return -1;

      va_list ap2;
      ap2 = ap; // MSVC では va_copy が無い場合があるので代入

      int len = _vscprintf(fmt, ap2);
      if (len < 0) {
          *strp = NULL;
          return -1;
      }

      char *buf = (char*)malloc((size_t)len + 1);
      if (!buf) {
          *strp = NULL;
          return -1;
      }

      vsnprintf(buf, (size_t)len + 1, fmt, ap);
      *strp = buf;
      return len;
  }

  int asprintf(char **strp, const char *fmt, ...)
  {
      va_list ap;
      va_start(ap, fmt);
      int ret = vasprintf(strp, fmt, ap);
      va_end(ap);
      return ret;
  }
#endif
