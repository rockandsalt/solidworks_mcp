// SolidWorksService.cpp : composition of the shared CAD executor. The RPCs are
// implemented in service/, one file per area; shared helpers are in
// service/*Helpers.h and service/Common.h.
#include "pch.h"
#include "SolidWorksService.h"

SolidWorksService::SolidWorksService(IGlobalInterfaceTable* git, DWORD swAppCookie)
	: m_executor(git, swAppCookie)
{
}
