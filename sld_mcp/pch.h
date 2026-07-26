// pch.h: This is a precompiled header file.
// Files listed below are compiled only once, improving build performance for future builds.
// This also affects IntelliSense performance, including code completion and many code browsing features.
// However, files listed here are ALL re-compiled if any one of them is updated between builds.
// Do not add files here that you will be updating frequently as this negates the performance advantage.

#ifndef PCH_H
#define PCH_H

// add headers that you want to pre-compile here
#include "framework.h"

// SolidWorks type libraries. $(SolidWorksInstallDir) is added as an
// additional include directory in the vcxproj so this resolves without a
// hardcoded path here.
#import "sldworks.tlb" raw_interfaces_only, raw_native_types, no_namespace, named_guids
#import "swpublished.tlb" raw_interfaces_only, raw_native_types, no_namespace, named_guids

#endif //PCH_H
