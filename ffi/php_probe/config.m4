PHP_ARG_ENABLE([rustpon_probe], [whether to enable the experimental rustpon request lifecycle probe],
    [AS_HELP_STRING([--enable-rustpon-probe], [Enable experimental rustpon RINIT/RSHUTDOWN probe])], [no])

if test "$PHP_RUSTPON_PROBE" != "no"; then
    PHP_NEW_EXTENSION([rustpon_probe], [rustpon_probe.c], [$ext_shared])
    PHP_ADD_INCLUDE([$ext_srcdir/../include])
    PHP_ADD_LIBRARY_WITH_PATH([rustpon_ffi], [$ext_srcdir/../target/debug], [RUSTPON_PROBE_SHARED_LIBADD])
    PHP_SUBST([RUSTPON_PROBE_SHARED_LIBADD])
fi
