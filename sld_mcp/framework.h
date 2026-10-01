#pragma once

#ifndef STRICT
#define STRICT
#endif

#include "targetver.h"

#define _ATL_APARTMENT_THREADED

#define _ATL_NO_AUTOMATIC_NAMESPACE

#define _ATL_CSTRING_EXPLICIT_CONSTRUCTORS	// some CString constructors will be explicit


#define ATL_NO_ASSERT_ON_DESTROY_NONEXISTENT_WINDOW

// grpc++ / protobuf / abseil compatibility. winsock2.h must come before
// windows.h (pulled in by atlbase.h), or windows.h drags in the old winsock.h
// and the two collide; and the min/max macros break std::numeric_limits.
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <winsock2.h>

#include "resource.h"
#include <atlbase.h>
#include <atlcom.h>
#include <atlctl.h>
