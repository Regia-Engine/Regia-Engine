<?xml version="1.0" encoding="UTF-8"?>
<tileset version="1.10" tiledversion="1.11.0" name="terrain" tilewidth="8" tileheight="8" tilecount="400" columns="20">
 <image source="tileset.png" width="160" height="160"/>
  <tile id="0" type="solid"/>
  <tile id="1" type="solid"/>
  <tile id="2" type="solid"/>
  <tile id="3" type="solid">
   <animation>
    <frame tileid="3" duration="200"/>
    <frame tileid="4" duration="200"/>
   </animation>
  </tile>
  <tile id="4" type="solid"/>
  <tile id="6">
   <properties>
    <property name="collision" value="one_way"/>
   </properties>
  </tile>
  <tile id="7">
   <properties>
    <property name="collision" value="one_way"/>
   </properties>
  </tile>
  <tile id="8">
   <properties>
    <property name="collision" value="one_way"/>
   </properties>
  </tile>
  <tile id="9">
   <properties>
    <property name="collision" value="one_way"/>
   </properties>
  </tile>
  <tile id="20" type="solid"/>
  <tile id="22" type="solid"/>
  <tile id="23" type="solid"/>
  <tile id="24" type="solid"/>
  <tile id="40" type="solid"/>
  <tile id="41" type="solid"/>
  <tile id="42" type="solid"/>
</tileset>
