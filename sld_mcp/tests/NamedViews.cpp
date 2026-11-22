#include "pch.h"
#include "service/DrawingHelpers.h"
#include <iostream>

// Standard views are found by their English name in any UI language: by the
// place SolidWorks lists them in. Other names match themselves in any case.
bool TestNamedViewIn()
{
	using sld_mcp::service::NamedViewIn;
	const std::vector<std::string> german = { "*Normal auf", "*Vorderseite", "*R\xC3\xBC" "ckseite", "*Links",
		"*Rechts", "*Oben", "*Unten", "*Isometrisch", "*Trimetrisch", "*Dimetrisch", "Saved" };
	std::vector<std::string> english = { "*Normal To", "*Front", "*Back", "*Left", "*Right", "*Top", "*Bottom",
		"*Isometric", "*Trimetric", "*Dimetric" };

	const bool germanOk = NamedViewIn(german, "*Front") == "*Vorderseite" &&
		NamedViewIn(german, "*Back") == german[2] && NamedViewIn(german, "*dimetric") == "*Dimetrisch" &&
		NamedViewIn(german, "*vorderseite") == "*Vorderseite" && NamedViewIn(german, "saved") == "Saved";
	const bool englishOk = NamedViewIn(english, "*front") == "*Front" && NamedViewIn(english, "*Isometric") == "*Isometric";
	// Unknown names, a list too short to hold the standard views, and one
	// whose first ten aren't all standard views give the name back for
	// SolidWorks to refuse.
	std::vector<std::string> saved = german;
	saved.insert(saved.begin() + 1, "Saved");
	saved.pop_back();
	const bool unknownOk = NamedViewIn(german, "nope") == "nope" && NamedViewIn({}, "*Front") == "*Front" &&
		NamedViewIn({ "*Normal auf", "*Vorderseite" }, "*Front") == "*Front" && NamedViewIn(saved, "*Front") == "*Front";

	if (!germanOk || !englishOk || !unknownOk)
	{
		std::cerr << "FAIL: named views spelled as the model has them (standard views by place, others by name)\n";
		return false;
	}
	std::cout << "PASS: named views spelled as the model has them (standard views by place, others by name)\n";
	return true;
}
