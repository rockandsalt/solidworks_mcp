// GrpcIncludes.h : the one place grpc++ / protobuf headers enter this project.
//
// Include it only from .cpp files on the gRPC side of the add-in
/// (RpcServer.cpp, SolidWorksService.cpp and service/*, via SolidWorksService.h) -- never
// from anything SwAddin.cpp or the rest of the COM plumbing includes, so
// that code keeps full /sdl checking.
//
// The warning pragmas deliberately stay in effect for the rest of the
// including translation unit: MSVC reports warnings raised inside template
// instantiations (which can happen as late as the end of the file) under the
// warning state in effect at that point, so a push/pop around the #includes
// wouldn't catch them.
//  - C4996: /sdl turns deprecation warnings into errors, and grpc++'s own
//    headers still call abseil/grpc APIs that have since been deprecated.
//  - C4018, C4099, C4244, C4267: sign/conversion noise from protobuf and abseil.
#pragma once

#pragma warning(disable : 4996 4018 4099 4244 4267)

#include <grpcpp/grpcpp.h>
#include <grpcpp/impl/service_type.h>
#include "sld_mcp.grpc.pb.h"
