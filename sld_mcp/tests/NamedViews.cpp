#include "pch.h"
#include "service/DrawingHelpers.h"
#include <catch2/catch_test_macros.hpp>

// Standard views are found by their English name in any UI language: by the
// place SolidWorks lists them in. Other names match themselves in any case.
TEST_CASE("named views spelled as the model has them (standard views by place, others by name)")
{
	using sld_mcp::service::NamedViewIn;
	const std::vector<std::string> german = { "*Normal auf", "*Vorderseite", "*R\xC3\xBC" "ckseite", "*Links",
		"*Rechts", "*Oben", "*Unten", "*Isometrisch", "*Trimetrisch", "*Dimetrisch", "Saved" };
	const std::vector<std::string> english = { "*Normal To", "*Front", "*Back", "*Left", "*Right", "*Top", "*Bottom",
		"*Isometric", "*Trimetric", "*Dimetric" };

	CHECK(NamedViewIn(german, "*Front") == "*Vorderseite");
	CHECK(NamedViewIn(german, "*Back") == german[2]);
	CHECK(NamedViewIn(german, "*dimetric") == "*Dimetrisch");
	CHECK(NamedViewIn(german, "*vorderseite") == "*Vorderseite");
	CHECK(NamedViewIn(german, "saved") == "Saved");
	CHECK(NamedViewIn(english, "*front") == "*Front");
	CHECK(NamedViewIn(english, "*Isometric") == "*Isometric");

	// Unknown names, a list too short to hold the standard views, and one
	// whose first ten aren't all standard views give the name back for
	// SolidWorks to refuse.
	std::vector<std::string> saved = german;
	saved.insert(saved.begin() + 1, "Saved");
	saved.pop_back();
	CHECK(NamedViewIn(german, "nope") == "nope");
	CHECK(NamedViewIn({}, "*Front") == "*Front");
	CHECK(NamedViewIn({ "*Normal auf", "*Vorderseite" }, "*Front") == "*Front");
	CHECK(NamedViewIn(saved, "*Front") == "*Front");
}
