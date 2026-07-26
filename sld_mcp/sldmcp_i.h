

/* this ALWAYS GENERATED file contains the definitions for the interfaces */


 /* File created by MIDL compiler version 8.01.0628 */
/* at Mon Jan 18 19:14:07 2038
 */
/* Compiler settings for sldmcp.idl:
    Oicf, W1, Zp8, env=Win64 (32b run), target_arch=AMD64 8.01.0628 
    protocol : all , ms_ext, c_ext, robust
    error checks: allocation ref bounds_check enum stub_data 
    VC __declspec() decoration level: 
         __declspec(uuid()), __declspec(selectany), __declspec(novtable)
         DECLSPEC_UUID(), MIDL_INTERFACE()
*/
/* @@MIDL_FILE_HEADING(  ) */



/* verify that the <rpcndr.h> version is high enough to compile this file*/
#ifndef __REQUIRED_RPCNDR_H_VERSION__
#define __REQUIRED_RPCNDR_H_VERSION__ 500
#endif

#include "rpc.h"
#include "rpcndr.h"

#ifndef __RPCNDR_H_VERSION__
#error this stub requires an updated version of <rpcndr.h>
#endif /* __RPCNDR_H_VERSION__ */


#ifndef __sldmcp_i_h__
#define __sldmcp_i_h__

#if defined(_MSC_VER) && (_MSC_VER >= 1020)
#pragma once
#endif

#ifndef DECLSPEC_XFGVIRT
#if defined(_CONTROL_FLOW_GUARD_XFG)
#define DECLSPEC_XFGVIRT(base, func) __declspec(xfg_virtual(base, func))
#else
#define DECLSPEC_XFGVIRT(base, func)
#endif
#endif

/* Forward Declarations */ 

#ifndef __SwMcpAddin_FWD_DEFINED__
#define __SwMcpAddin_FWD_DEFINED__

#ifdef __cplusplus
typedef class SwMcpAddin SwMcpAddin;
#else
typedef struct SwMcpAddin SwMcpAddin;
#endif /* __cplusplus */

#endif 	/* __SwMcpAddin_FWD_DEFINED__ */


/* header files for imported files */
#include "oaidl.h"
#include "ocidl.h"

#ifdef __cplusplus
extern "C"{
#endif 



#ifndef __sldmcpLib_LIBRARY_DEFINED__
#define __sldmcpLib_LIBRARY_DEFINED__

/* library sldmcpLib */
/* [version][uuid] */ 


EXTERN_C const IID LIBID_sldmcpLib;

EXTERN_C const CLSID CLSID_SwMcpAddin;

#ifdef __cplusplus

class DECLSPEC_UUID("c191de66-c755-4b06-928b-c39f69d1afc7")
SwMcpAddin;
#endif
#endif /* __sldmcpLib_LIBRARY_DEFINED__ */

/* Additional Prototypes for ALL interfaces */

/* end of Additional Prototypes */

#ifdef __cplusplus
}
#endif

#endif


