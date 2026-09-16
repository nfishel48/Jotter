#ifndef __SPEEX_TYPES_H__
#define __SPEEX_TYPES_H__

/* Hand-written replacement for upstream's speexdsp_config_types.h.in, which is
 * an autotools template: `configure` substitutes @INCLUDE_STDINT@ and the four
 * @SIZE*@ placeholders after probing the platform. We do not run configure, so
 * without this file `speexdsp_types.h:122` fails to resolve its include.
 *
 * Probing is unnecessary anyway: every target jotter builds for has a C99
 * <stdint.h>, which defines these widths exactly. Anything that does not is
 * already excluded by cpal.
 */

#include <stdint.h>

typedef int16_t spx_int16_t;
typedef uint16_t spx_uint16_t;
typedef int32_t spx_int32_t;
typedef uint32_t spx_uint32_t;

#endif
