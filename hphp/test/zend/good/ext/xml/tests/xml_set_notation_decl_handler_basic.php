<?hh
/* Prototype  : proto bool xml_set_notation_decl_handler  ( resource $parser  , callback $handler  )
 * Description: Sets the notation declaration handler function for the XML parser.
 * Source code: ext/xml/xml.c
 * Alias to functions:
 */

class XML_Parser
{

    function unparsed_entity_decl_handler($parser, $entity_name, $base, $system_ID, $public_ID, $notation_name)
      {
          echo "unparsed_entity_decl_handler called\n";
         echo "...Entity name=" . $entity_name . "\n";
         echo "...Base=" . (string)($base) . "\n";
         echo "...System ID=" . $system_ID . "\n";
         echo "...Public ID=" . (string)($public_ID) . "\n";
         echo "...Notation name=" . $notation_name . "\n";
      }

      function notation_decl_handler($parser, $name, $base, $system_ID,$public_ID)
      {
          echo "notation_decl_handler called\n";
         echo "...Name=" . $name . "\n";
         echo "...Base=" . (string)($base) . "\n";
         echo "...System ID=" . $system_ID . "\n";
         echo "...Public ID=" . (string)($public_ID) . "\n";
      }

    function parse($data)
    {
        $parser = xml_parser_create();
        xml_set_object($parser, $this);
        xml_set_notation_decl_handler($parser, "notation_decl_handler");
        xml_set_unparsed_entity_decl_handler($parser, "unparsed_entity_decl_handler");
        xml_parse($parser, $data, true);
        xml_parser_free($parser);
    }
}
<<__EntryPoint>> function main(): void {
$xml = <<<HERE
<?xml version="1.0"?>
<!DOCTYPE dates [
    <!NOTATION USDATE SYSTEM "http://www.schema.net/usdate.not">
    <!NOTATION AUSDATE SYSTEM "http://www.schema.net/ausdate.not">
    <!NOTATION ISODATE SYSTEM "http://www.schema.net/isodate.not">
    <!ENTITY testUS  SYSTEM "test_usdate.xml" NDATA USDATE>
    <!ENTITY testAUS SYSTEM "test_ausdate.xml" NDATA AUSDATE>
    <!ENTITY testISO SYSTEM "test_isodate_xml" NDATA ISODATE>]>
]>
HERE;

echo "Simple test of xml_set_notation_decl_handler(() function\n";
$p1 = new XML_Parser();
$p1->parse($xml);
echo "Done\n";
}
